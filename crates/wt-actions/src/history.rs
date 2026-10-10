use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::{fs, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wt_platform::lock::FileLock;

use crate::service::ActionServiceError;

const MAX_PER_ACTION: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionHistoryEntry {
    pub value: String,
    pub label: Option<String>,
    pub ts: i64,
}

/// Read the newest values for one action. A missing/corrupt optional picker
/// history is an empty list, matching the legacy UX; writes still fail closed.
pub async fn recent_values(path: &Path, action_id: &str) -> Vec<ActionHistoryEntry> {
    let Ok(bytes) = fs::read(path).await else {
        return Vec::new();
    };
    let Ok(value): Result<Value, _> = serde_json::from_slice(&bytes) else {
        return Vec::new();
    };
    let Some(entries) = value.get(action_id).and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
        .take(MAX_PER_ACTION)
        .collect()
}

/// Prepend and deduplicate a picker value under the shared action-history
/// lock. Unknown action IDs and fields remain untouched.
pub async fn record_value(
    path: &Path,
    action_id: &str,
    value: &str,
    label: Option<&str>,
    now_ms: i64,
    cancellation: &CancellationToken,
) -> Result<(), ActionServiceError> {
    record_value_marked(path, action_id, value, label, now_ms, None, cancellation).await
}

/// Record a launch and attach its durable run identity in one locked write.
pub async fn record_value_for_run(
    path: &Path,
    action_id: &str,
    value: &str,
    label: Option<&str>,
    now_ms: i64,
    run_id: &str,
    cancellation: &CancellationToken,
) -> Result<(), ActionServiceError> {
    record_value_marked(
        path,
        action_id,
        value,
        label,
        now_ms,
        Some(run_id),
        cancellation,
    )
    .await
}

async fn record_value_marked(
    path: &Path,
    action_id: &str,
    value: &str,
    label: Option<&str>,
    now_ms: i64,
    run_id: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<(), ActionServiceError> {
    if value.is_empty() {
        return Ok(());
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("action-history");
    let lock_key = filename.replace(['/', '\\'], "_");
    let _lock = FileLock::acquire(parent, &lock_key, "record action history", cancellation).await?;
    let mut store = match fs::read(path).await {
        Ok(bytes) => {
            serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(source) => return Err(io_error("read action history", path, source)),
    };
    if !store.is_object() {
        store = Value::Object(Map::new());
    }
    let values = store.as_object_mut().expect("object initialized");
    let prior = values
        .get(action_id)
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.get("value").and_then(Value::as_str) == Some(value))
        });
    let preserved_label = label.map(str::to_owned).or_else(|| {
        prior.and_then(|entry| {
            entry
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    });
    let mut next = prior
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    next.insert("value".into(), Value::String(value.to_owned()));
    next.insert(
        "label".into(),
        preserved_label.map_or(Value::Null, Value::String),
    );
    next.insert("ts".into(), Value::from(now_ms));
    if let Some(run_id) = run_id {
        next.insert("launchToken".into(), Value::String(run_id.to_owned()));
    }
    let next = Value::Object(next);
    let mut entries = vec![next];
    if let Some(old) = values.get(action_id).and_then(Value::as_array) {
        entries.extend(
            old.iter()
                .filter(|entry| entry.get("value").and_then(Value::as_str) != Some(value))
                .take(MAX_PER_ACTION - 1)
                .cloned(),
        );
    }
    values.insert(action_id.to_owned(), Value::Array(entries));
    write_atomic(path, &store).await
}

/// Refine a launch-time value without changing its recency or moving it ahead
/// of a newer invocation. This makes late worker completion safe when a newer
/// action has already updated the same history file.
pub async fn refine_value(
    path: &Path,
    action_id: &str,
    value: &str,
    label: &str,
    launched_at: i64,
    run_id: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<bool, ActionServiceError> {
    if value.is_empty() || label.is_empty() || value == label {
        return Ok(false);
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("action-history");
    let lock_key = filename.replace(['/', '\\'], "_");
    let _lock = FileLock::acquire(parent, &lock_key, "refine action history", cancellation).await?;
    let mut store = match fs::read(path).await {
        Ok(bytes) => {
            serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(io_error("read action history", path, source)),
    };
    let Some(entries) = store.get_mut(action_id).and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.get("value").and_then(Value::as_str) == Some(value))
    else {
        return Ok(false);
    };
    let entry_ts = entry.get("ts").and_then(Value::as_i64).unwrap_or(i64::MIN);
    let owned_entry = run_id
        .is_some_and(|run_id| entry.get("launchToken").and_then(Value::as_str) == Some(run_id));
    if !owned_entry && (run_id.is_some() || entry_ts > launched_at) {
        return Ok(false);
    }
    let Some(entry) = entry.as_object_mut() else {
        return Ok(false);
    };
    entry.insert("label".into(), Value::String(label.to_owned()));
    write_atomic(path, &store).await?;
    Ok(true)
}

async fn write_atomic(path: &Path, value: &Value) -> Result<(), ActionServiceError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .await
        .map_err(|source| io_error("create action-history directory", parent, source))?;
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp: PathBuf = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|source| ActionServiceError::InvalidJob {
            path: path.to_path_buf(),
            message: source.to_string(),
        })?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .await
        .map_err(|source| io_error("create action-history temporary", &temp, source))?;
    file.write_all(&bytes)
        .await
        .map_err(|source| io_error("write action history", &temp, source))?;
    file.sync_all()
        .await
        .map_err(|source| io_error("sync action history", &temp, source))?;
    drop(file);
    fs::rename(&temp, path)
        .await
        .map_err(|source| io_error("replace action history", path, source))?;
    Ok(())
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> ActionServiceError {
    ActionServiceError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recording_deduplicates_caps_and_preserves_other_action_data() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("action-history.json");
        fs::write(&path, br#"{"other":[{"value":"keep","label":null,"ts":1}],"x":[{"value":"one","label":"One","ts":1}]}"#).await.unwrap();
        let cancel = CancellationToken::new();
        record_value(&path, "x", "one", None, 2, &cancel)
            .await
            .unwrap();
        assert_eq!(
            recent_values(&path, "x").await[0].label.as_deref(),
            Some("One")
        );
        for index in 0..12 {
            record_value(&path, "x", &format!("v{index}"), None, 3 + index, &cancel)
                .await
                .unwrap();
        }
        let values = recent_values(&path, "x").await;
        assert_eq!(values.len(), MAX_PER_ACTION);
        assert_eq!(values[0].value, "v11");
        assert_eq!(values.last().unwrap().value, "v2");
        let root: Value = serde_json::from_slice(&fs::read(path).await.unwrap()).unwrap();
        assert_eq!(root["other"][0]["value"], "keep");
    }

    #[tokio::test]
    async fn late_refinement_preserves_newer_order_and_unknown_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("action-history.json");
        fs::write(
            &path,
            br#"{"other":{"kept":true},"x":[{"value":"new","label":null,"ts":20,"future":2},{"value":"old","label":null,"ts":10,"future":1}]}"#,
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        assert!(
            refine_value(&path, "x", "old", "Old label", 10, None, &cancel)
                .await
                .unwrap()
        );
        let root: Value = serde_json::from_slice(&fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(root["other"]["kept"], true);
        assert_eq!(root["x"][0]["value"], "new");
        assert_eq!(root["x"][1]["label"], "Old label");
        assert_eq!(root["x"][1]["future"], 1);
        assert_eq!(root["x"][1]["ts"], 10);
    }

    #[tokio::test]
    async fn stale_same_value_refinement_does_not_overwrite_newer_launch() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("action-history.json");
        let cancel = CancellationToken::new();
        record_value(&path, "x", "same", None, 20, &cancel)
            .await
            .unwrap();
        assert!(
            !refine_value(&path, "x", "same", "stale", 10, None, &cancel)
                .await
                .unwrap()
        );
        assert_eq!(recent_values(&path, "x").await[0].label, None);
    }

    #[tokio::test]
    async fn run_token_keeps_late_duplicate_value_refinement_from_clobbering_new_run() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("action-history.json");
        let cancel = CancellationToken::new();
        record_value_for_run(&path, "x", "same", None, 20, "new-run", &cancel)
            .await
            .unwrap();
        assert!(
            !refine_value(
                &path,
                "x",
                "same",
                "stale label",
                10,
                Some("old-run"),
                &cancel,
            )
            .await
            .unwrap()
        );
        assert_eq!(recent_values(&path, "x").await[0].label, None);
        let raw: Value = serde_json::from_slice(&fs::read(path).await.unwrap()).unwrap();
        assert_eq!(raw["x"][0]["launchToken"], "new-run");
    }
}
