use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::context::AppContext;

#[derive(Debug, Clone, Args, Default)]
pub struct StateArgs {
    #[command(subcommand)]
    pub command: Option<StateCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum StateCommand {
    Migrate {
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        keep_legacy: bool,
    },
}

pub async fn run(ctx: &AppContext, args: &StateArgs) -> Result<i32> {
    let Some(StateCommand::Migrate { from, keep_legacy }) = &args.command else {
        eprintln!("usage: wt state migrate [--from <legacy-cache-dir>] [--keep-legacy]");
        return Ok(2);
    };
    let dir = from.clone().unwrap_or_else(|| ctx.home.join(".cache/wt"));
    let inventory = ctx.repository.inventory(&ctx.cancellation).await?;
    let live = inventory
        .iter()
        .map(|row| (row.target.slug().to_owned(), row.target.branch.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let state_path = dir.join("state.json");
    let archive_path = dir.join("archive.json");
    let lock_source = dir.clone();
    let migration_lock_dir =
        tokio::task::spawn_blocking(move || migration_source_lock_dir(&lock_source)).await?;
    let _migration_lock = wt_platform::lock::FileLock::acquire(
        &migration_lock_dir,
        "legacy-state-migration",
        "migrate legacy state",
        &ctx.cancellation,
    )
    .await?;
    let read_state_path = state_path.clone();
    let read_archive_path = archive_path.clone();
    let (legacy, archive) = tokio::task::spawn_blocking(move || -> Result<(Value, Value)> {
        let legacy = if read_state_path.exists() {
            serde_json::from_slice::<Value>(
                &fs::read(&read_state_path)
                    .with_context(|| format!("read {}", read_state_path.display()))?,
            )?
        } else {
            empty_state()
        };
        let archive = if read_archive_path.exists() {
            serde_json::from_slice::<Value>(
                &fs::read(&read_archive_path)
                    .with_context(|| format!("read {}", read_archive_path.display()))?,
            )?
        } else {
            json!({"slugs": []})
        };
        Ok((legacy, archive))
    })
    .await??;
    let mut selected = HashSet::new();
    let imported = project_state(&legacy, &live, &mut selected);
    let remote_prefix = ctx
        .config
        .remote
        .as_ref()
        .map(|remote| wt_core::remote_worktree_ledger_prefix(&remote.host));
    let mut selected_archive = archive
        .get("slugs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| {
            live.contains_key(*key)
                || remote_prefix
                    .as_ref()
                    .is_some_and(|prefix| key.starts_with(prefix))
        })
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let home = ctx.home.clone();
    let state_db = ctx.config.paths.state_db.clone();
    let repo_id = ctx.config.repo_id.clone();
    let repo_path = canonical_path(&ctx.config.repo_path);
    let worktree_root = canonical_path(&ctx.config.paths.worktree_root);
    let current_db = canonical_path(&ctx.config.paths.state_db);
    let stranded = tokio::task::spawn_blocking(move || -> Result<Vec<StrandedRow>> {
        let mut rows = Vec::new();
        for source in candidate_state_databases(&home, &state_db) {
            for row in wt_store::Store::read_foreign_repository_rows(&source)? {
                let canonical_source = canonical_path(&source);
                rows.push(StrandedRow {
                    row,
                    source: canonical_source,
                });
            }
        }
        Ok(select_stranded_rows(
            rows,
            &repo_id,
            &repo_path,
            &worktree_root,
            &current_db,
        ))
    })
    .await??;
    let mut adopted_from = Vec::new();
    let mut imported = imported;
    for stray in stranded {
        let Ok(raw) = serde_json::from_str::<Value>(&stray.row.data) else {
            continue;
        };
        let mut stray_slugs = HashSet::new();
        let selected_stray = project_state(&raw, &live, &mut stray_slugs);
        let live_keys = live.keys().cloned().collect::<BTreeSet<_>>();
        let archive_keys = stray
            .row
            .archived
            .intersection(&live_keys)
            .cloned()
            .collect::<Vec<_>>();
        if stray_slugs.is_empty() && archive_keys.is_empty() {
            continue;
        }
        imported = merge_adopted_state(selected_stray, imported);
        selected_archive.extend(archive_keys);
        adopted_from.push(stray.row.repo_id);
    }
    let backup_state_path = state_path.clone();
    let backup_archive_path = archive_path.clone();
    let backups = tokio::task::spawn_blocking(move || {
        backup_sources(&[backup_state_path, backup_archive_path])
    })
    .await??;
    let archive_for_import = selected_archive.clone();
    ctx.database
        .call(move |store| {
            store.merge_repository_snapshot(&archive_for_import, |current| {
                let current_exists = current.is_some();
                let merged = merge_state(
                    imported,
                    current.unwrap_or_else(empty_state),
                    current_exists,
                );
                Ok(merged)
            })?;
            Ok(())
        })
        .await?;
    if !keep_legacy {
        let prune_selected = selected.clone();
        let prune_archive = selected_archive.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if state_path.exists() {
                let mut remaining = legacy;
                let slugs = remaining
                    .get("slugs")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(slug, _)| !prune_selected.contains(slug))
                    .collect::<Map<_, _>>();
                remaining["slugs"] = Value::Object(slugs);
                let retained_edges = remaining
                    .get("edges")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|edge| {
                        let from_selected = edge
                            .get("from")
                            .and_then(Value::as_str)
                            .is_some_and(|slug| prune_selected.contains(slug));
                        let to_selected = edge
                            .get("to")
                            .and_then(Value::as_str)
                            .is_some_and(|slug| prune_selected.contains(slug));
                        !(from_selected && to_selected)
                    })
                    .cloned()
                    .collect();
                remaining["edges"] = Value::Array(retained_edges);
                atomic_json(&state_path, &remaining)?;
            }
            if archive_path.exists() {
                let mut remaining = archive;
                let keys = remaining
                    .get("slugs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|key| key.as_str().is_none_or(|key| !prune_archive.contains(key)))
                    .collect::<Vec<_>>();
                remaining["slugs"] = Value::Array(keys);
                atomic_json(&archive_path, &remaining)?;
            }
            Ok(())
        })
        .await??;
    }
    println!(
        "migrated {} live worktree records into {}",
        selected.len(),
        ctx.config.paths.state_db.display()
    );
    if !backups.is_empty() {
        println!(
            "backups: {}",
            backups
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if *keep_legacy {
        println!("legacy files retained (--keep-legacy)");
    }
    if !adopted_from.is_empty() {
        println!(
            "adopted {} stranded namespace(s) without modifying their source databases",
            adopted_from.len()
        );
    }
    let carry_from = dir.clone();
    let cache_root = ctx.config.paths.cache_root.clone();
    tokio::task::spawn_blocking(move || carry_legacy_runtime(&carry_from, &cache_root)).await??;
    Ok(0)
}

fn empty_state() -> Value {
    json!({"version": wt_store::CURRENT_WT_STATE_VERSION, "slugs": {}, "sectionsOrder": [], "foldedSections": [], "pausedStacks": [], "removed": [], "edges": [], "branchTips": {}, "automationsPaused": false, "attentionSeenTs": 0})
}

fn backup_sources(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut backups = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let suffix = path.extension().and_then(|x| x.to_str()).unwrap_or("json");
        let backup = path.with_extension(format!("{suffix}.bak-sqlite-{stamp}"));
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
            .with_context(|| format!("reserve backup {}", backup.display()))?;
        let copied = (|| -> Result<()> {
            let mut source = fs::File::open(path)?;
            std::io::copy(&mut source, &mut destination)?;
            destination.sync_all()?;
            Ok(())
        })();
        if let Err(error) = copied {
            let _ = fs::remove_file(&backup);
            return Err(error).with_context(|| format!("back up {}", path.display()));
        }
        backups.push(backup);
    }
    Ok(backups)
}

fn project_state(
    raw: &Value,
    live: &std::collections::BTreeMap<String, String>,
    selected: &mut HashSet<String>,
) -> Value {
    let mut state = raw.clone();
    let mut slugs = Map::new();
    if let Some(records) = raw.get("slugs").and_then(Value::as_object) {
        for (slug, value) in records {
            if live.contains_key(slug) {
                selected.insert(slug.clone());
                slugs.insert(slug.clone(), value.clone());
            }
        }
    }
    state["slugs"] = Value::Object(slugs);
    let selected_slugs = state["slugs"].as_object().cloned().unwrap_or_default();
    let owns_section = |name: &str| {
        name == "\0inbox"
            || name == "\0archived"
            || name
                .strip_prefix("\0stack:")
                .is_some_and(|branch| live.values().any(|candidate| candidate == branch))
            || selected_slugs
                .values()
                .any(|record| record.get("section").and_then(Value::as_str) == Some(name))
    };
    for key in ["sectionsOrder", "foldedSections"] {
        if let Some(items) = raw.get(key).and_then(Value::as_array) {
            state[key] = Value::Array(
                items
                    .iter()
                    .filter(|item| item.as_str().is_some_and(&owns_section))
                    .cloned()
                    .collect(),
            );
        }
    }
    state["pausedStacks"] = Value::Array(
        raw.get("pausedStacks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| {
                item.as_str()
                    .is_some_and(|branch| live.values().any(|candidate| candidate == branch))
            })
            .cloned()
            .collect(),
    );
    state["removed"] = json!([]);
    state["branchTips"] = json!({});
    state["edges"] = Value::Array(
        raw.get("edges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|edge| {
                edge.get("from")
                    .and_then(Value::as_str)
                    .is_some_and(|slug| selected.contains(slug))
                    && edge
                        .get("to")
                        .and_then(Value::as_str)
                        .is_some_and(|slug| selected.contains(slug))
            })
            .cloned()
            .collect(),
    );
    state
}

fn merge_state(legacy: Value, current: Value, current_exists: bool) -> Value {
    let mut merged = legacy.clone();
    if let (Some(to), Some(from)) = (merged.as_object_mut(), current.as_object()) {
        for (key, value) in from {
            to.insert(key.clone(), value.clone());
        }
    }
    let version = legacy
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .max(current.get("version").and_then(Value::as_u64).unwrap_or(0));
    if version > 0 {
        merged["version"] = json!(version);
    }
    for key in ["slugs"] {
        let mut values = legacy
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(now) = current.get(key).and_then(Value::as_object) {
            for (slug, value) in now {
                let Some(previous) = values.get(slug) else {
                    values.insert(slug.clone(), value.clone());
                    continue;
                };
                let (Some(previous_fields), Some(current_fields)) =
                    (previous.as_object(), value.as_object())
                else {
                    values.insert(slug.clone(), value.clone());
                    continue;
                };
                let mut merged_fields = previous_fields.clone();
                for (field, current_value) in current_fields {
                    merged_fields.insert(field.clone(), current_value.clone());
                }
                if value.get("section").is_some_and(Value::is_null)
                    && previous
                        .get("section")
                        .is_some_and(|section| !section.is_null())
                {
                    merged_fields.insert("section".into(), previous["section"].clone());
                }
                values.insert(slug.clone(), Value::Object(merged_fields));
            }
        }
        merged[key] = Value::Object(values);
    }
    for key in ["sectionsOrder", "foldedSections", "pausedStacks"] {
        let mut values = Vec::new();
        for value in current
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(
                legacy
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
        {
            if !values.contains(value) {
                values.push(value.clone());
            }
        }
        merged[key] = Value::Array(values);
    }
    for key in ["removed", "branchTips"] {
        merged[key] = current.get(key).cloned().unwrap_or_else(|| {
            if key == "removed" {
                json!([])
            } else {
                json!({})
            }
        });
    }
    if !current_exists {
        for key in ["automationsPaused", "attentionSeenTs"] {
            if let Some(value) = legacy.get(key) {
                merged[key] = value.clone();
            }
        }
    }
    let mut edges = BTreeMap::new();
    let edge_key = |edge: &Value| {
        (
            edge.get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            edge.get("from")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            edge.get("to")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )
    };
    for edge in legacy
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        edges.insert(edge_key(edge), edge.clone());
    }
    for edge in current
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        edges.insert(edge_key(edge), edge.clone());
    }
    merged["edges"] = Value::Array(edges.into_values().collect());
    merged
}

#[derive(Debug)]
struct StrandedRow {
    row: wt_store::ForeignRepositoryRow,
    source: PathBuf,
}

fn select_stranded_rows(
    mut rows: Vec<StrandedRow>,
    repo_id: &str,
    repo_path: &Path,
    worktree_root: &Path,
    current_db: &Path,
) -> Vec<StrandedRow> {
    let repo_path = canonical_path(repo_path);
    let worktree_root = canonical_path(worktree_root);
    let current_db = canonical_path(current_db);
    rows.retain(|candidate| {
        let current =
            candidate.row.repo_id == repo_id && canonical_path(&candidate.source) == current_db;
        let path = canonical_path(Path::new(&candidate.row.repo_path));
        !current && (path == repo_path || path.starts_with(&worktree_root))
    });
    rows.sort_by_key(|candidate| candidate.row.updated_at);
    rows
}

fn candidate_state_databases(home: &Path, current: &Path) -> Vec<PathBuf> {
    let cache_root = home.join(".cache/wt");
    let mut paths = vec![
        current.to_path_buf(),
        home.join(".local/state/wt/wt.sqlite"),
        cache_root.join("wt.sqlite"),
    ];
    if let Ok(entries) = fs::read_dir(&cache_root) {
        for entry in entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        {
            paths.push(entry.path().join("wt.sqlite"));
        }
    }
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(canonical_path(path)));
    paths
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    })
}

fn migration_source_lock_dir(source_dir: &Path) -> PathBuf {
    canonical_path(source_dir)
}

fn merge_adopted_state(stray: Value, current: Value) -> Value {
    let mut merged = merge_state(stray.clone(), current.clone(), true);
    let mut slugs = merged
        .get("slugs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (slug, stray_record) in stray
        .get("slugs")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(current_record) = current
            .get("slugs")
            .and_then(Value::as_object)
            .and_then(|values| values.get(slug))
        else {
            slugs.insert(slug.clone(), stray_record.clone());
            continue;
        };
        let mut filled = current_record.as_object().cloned().unwrap_or_default();
        if let Some(stray_fields) = stray_record.as_object() {
            for (key, value) in stray_fields {
                filled.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        if current_record.get("section").is_some_and(Value::is_null)
            && stray_record
                .get("section")
                .is_some_and(|value| !value.is_null())
        {
            filled.insert("section".into(), stray_record["section"].clone());
        }
        slugs.insert(slug.clone(), Value::Object(filled));
    }
    merged["slugs"] = Value::Object(slugs);
    merged
}

fn carry_legacy_runtime(from: &Path, to: &Path) -> Result<()> {
    if canonical_path(from) == canonical_path(to) {
        return Ok(());
    }
    for name in ["automations.json", "harness.json"] {
        let source = from.join(name);
        let destination = to.join(name);
        if source.exists() && !destination.exists() {
            fs::create_dir_all(to)?;
            fs::copy(source, destination)?;
        }
    }
    for name in [
        "claude-sessions.json",
        "codex-sessions.json",
        "opencode-sessions.json",
    ] {
        let source = from.join(name);
        if !source.exists() {
            continue;
        }
        let Ok(legacy) = serde_json::from_slice::<Value>(&fs::read(&source)?) else {
            continue;
        };
        let destination = to.join(name);
        let current = if destination.exists() {
            serde_json::from_slice(&fs::read(&destination)?).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let merged = merge_registry(legacy, current.clone());
        if merged != current {
            fs::create_dir_all(to)?;
            atomic_json(&destination, &merged)?;
        }
    }
    Ok(())
}

fn merge_registry(legacy: Value, current: Value) -> Value {
    match (legacy, current) {
        (Value::Array(left), Value::Array(right)) => {
            let mut result = right;
            for value in left {
                if !result.contains(&value) {
                    result.push(value);
                }
            }
            Value::Array(result)
        }
        (Value::Object(left), Value::Object(mut right)) => {
            for (key, value) in left {
                let merged = right
                    .remove(&key)
                    .map(|current| merge_registry(value.clone(), current))
                    .unwrap_or(value);
                right.insert(key, merged);
            }
            Value::Object(right)
        }
        (legacy, current) => {
            if current.is_null() {
                legacy
            } else {
                current
            }
        }
    }
}

fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!(
        "{}.{}.{}.tmp",
        path.extension().and_then(|x| x.to_str()).unwrap_or("json"),
        std::process::id(),
        stamp
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    use std::io::Write;
    file.write_all(format!("{}\n", serde_json::to_string_pretty(value)?).as_bytes())?;
    file.sync_all()?;
    fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_projects_only_live_rows_and_preserves_unknown_fields() {
        let raw = json!({"version":17,"future":42,"slugs":{"live":{"section":"alpha"},"foreign":{"issueId":"X-1"}},"sectionsOrder":["alpha","missing","\0inbox","\0stack:branch/live"],"edges":[{"from":"live","to":"foreign"}]});
        let live = [("live".into(), "branch/live".into())]
            .into_iter()
            .collect();
        let mut selected = HashSet::new();
        let migrated = project_state(&raw, &live, &mut selected);
        assert_eq!(selected, ["live".to_owned()].into_iter().collect());
        assert_eq!(migrated["future"], 42);
        assert_eq!(
            migrated["sectionsOrder"],
            json!(["alpha", "\0inbox", "\0stack:branch/live"])
        );
        assert!(migrated["slugs"].get("foreign").is_none());
        assert_eq!(migrated["edges"].as_array().unwrap().len(), 0);
    }
    #[test]
    fn current_repository_values_win_and_unknown_fields_survive() {
        let result = merge_state(
            json!({"future":1,"slugs":{"x":{"issueId":"A-1"}}}),
            json!({"slugs":{"x":{"issueId":"B-2"}}}),
            true,
        );
        assert_eq!(result["slugs"]["x"]["issueId"], "B-2");
        assert_eq!(result["future"], 1);
    }

    #[test]
    fn migration_never_downgrades_a_newer_legacy_state_envelope() {
        let merged = merge_state(
            json!({"version":99,"future":true}),
            json!({"version":17,"current":true}),
            true,
        );
        assert_eq!(merged["version"], 99);
        assert_eq!(merged["future"], true);
        assert_eq!(merged["current"], true);
    }

    #[test]
    fn current_merge_edge_replaces_legacy_edge_with_the_same_identity() {
        let merged = merge_state(
            json!({"edges":[{"kind":"before","from":"a","to":"b","why":"old"}]}),
            json!({"edges":[{"kind":"before","from":"a","to":"b","why":"current"}]}),
            true,
        );
        assert_eq!(merged["edges"][0]["why"], "current");
    }

    #[test]
    fn stranded_sibling_fills_missing_fields_but_current_empty_issue_override_wins() {
        let stray = json!({"slugs":{"one":{"issueId":"COZ-2","section":"Old","baseBranch":"main"},"two":{"issueId":"X-1"}}});
        let current = json!({"slugs":{"one":{"issueId":"","section":null,"title":"Pinned"}}});
        let merged = merge_adopted_state(stray, current);
        assert_eq!(merged["slugs"]["one"]["issueId"], "");
        assert_eq!(merged["slugs"]["one"]["section"], "Old");
        assert_eq!(merged["slugs"]["one"]["title"], "Pinned");
        assert_eq!(merged["slugs"]["one"]["baseBranch"], "main");
        assert_eq!(merged["slugs"]["two"]["issueId"], "X-1");
    }

    #[test]
    fn only_stranded_repo_paths_are_adopted_and_source_databases_are_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let current_db = tmp.path().join("current.sqlite");
        let source = tmp.path().join("old.sqlite");
        let inside = tmp.path().join("worktrees/one");
        let outside = tmp.path().join("elsewhere");
        for path in [&inside, &outside] {
            fs::create_dir_all(path).unwrap();
        }
        let identity = wt_store::RepositoryIdentity::new("stray", inside.to_string_lossy());
        {
            let mut store = wt_store::Store::open(&source, identity).unwrap();
            store
                .write_repository_state_json(r#"{"version":17,"slugs":{"one":{"issueId":"X-1"}}}"#)
                .unwrap();
            store.set_archived("one", true).unwrap();
        }
        let rows = wt_store::Store::read_foreign_repository_rows(&source)
            .unwrap()
            .into_iter()
            .map(|row| StrandedRow {
                row,
                source: source.clone(),
            })
            .collect();
        let selected = select_stranded_rows(
            rows,
            "current",
            tmp.path(),
            &tmp.path().join("worktrees"),
            &current_db,
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].row.repo_path, inside.to_string_lossy());
        assert_eq!(
            wt_store::Store::read_foreign_repository_rows(&source).unwrap()[0].archived,
            ["one".to_owned()].into_iter().collect()
        );
        let _ = outside;
    }

    #[tokio::test]
    async fn repositories_with_distinct_lock_dirs_serialize_migration_of_shared_source() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("shared-legacy");
        let first_repo_locks = tmp.path().join("first-locks");
        let second_repo_locks = tmp.path().join("second-locks");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&first_repo_locks).unwrap();
        fs::create_dir_all(&second_repo_locks).unwrap();

        let first_lock_dir = migration_source_lock_dir(&source);
        let second_lock_dir = migration_source_lock_dir(&source);
        assert_ne!(first_repo_locks, second_repo_locks);
        assert_eq!(first_lock_dir, second_lock_dir);
        let first = wt_platform::lock::FileLock::try_acquire(
            &first_lock_dir,
            "legacy-state-migration",
            "migrate legacy state",
        )
        .await
        .unwrap()
        .expect("first repository owns the shared source lock");
        assert!(
            wt_platform::lock::FileLock::try_acquire(
                &second_lock_dir,
                "legacy-state-migration",
                "migrate legacy state",
            )
            .await
            .unwrap()
            .is_none(),
            "another repository must wait while a migration can prune the source"
        );
        drop(first);
        assert!(
            wt_platform::lock::FileLock::try_acquire(
                &second_lock_dir,
                "legacy-state-migration",
                "migrate legacy state",
            )
            .await
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn session_registry_merge_preserves_both_names_and_prefers_destination_conflicts() {
        let merged = merge_registry(
            json!({"one":{"old":"a","shared":"source"},"two":["legacy"]}),
            json!({"one":{"shared":"current"},"two":["current"]}),
        );
        assert_eq!(merged["one"]["old"], "a");
        assert_eq!(merged["one"]["shared"], "current");
        assert_eq!(merged["two"], json!(["current", "legacy"]));
    }

    #[tokio::test]
    async fn migrate_backups_before_import_prunes_only_imported_rows_and_keeps_unknown_fields() {
        use crate::commands::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let legacy_dir = fixture._root.path().join("legacy");
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("state.json"), json!({"version":17,"futureRoot":"retain","slugs":{"one":{"issueId":"COZ-2"},"foreign":{"issueId":"NO-1"}}}).to_string()).unwrap();
        fs::write(
            legacy_dir.join("archive.json"),
            json!({"futureArchive":true,"slugs":["one","foreign"]}).to_string(),
        )
        .unwrap();
        let args = StateArgs {
            command: Some(StateCommand::Migrate {
                from: Some(legacy_dir.clone()),
                keep_legacy: false,
            }),
        };
        assert_eq!(run(&fixture.ctx, &args).await.unwrap(), 0);
        let migrated = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(migrated["slugs"]["one"]["issueId"], "COZ-2");
        assert!(migrated["slugs"].get("foreign").is_none());
        let remaining: Value =
            serde_json::from_slice(&fs::read(legacy_dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(remaining["futureRoot"], "retain");
        assert!(remaining["slugs"].get("foreign").is_some());
        let archive: Value =
            serde_json::from_slice(&fs::read(legacy_dir.join("archive.json")).unwrap()).unwrap();
        assert_eq!(archive["futureArchive"], true);
        assert_eq!(archive["slugs"], json!(["foreign"]));
        assert!(legacy_dir.join("state.json").exists());
        assert!(std::fs::read_dir(&legacy_dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("bak-sqlite-")
        }));
        fixture.close().await.unwrap();
    }
}
