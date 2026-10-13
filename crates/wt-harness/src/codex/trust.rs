//! Exact-root trust for checkouts owned by wt's inventory.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table};

use crate::persist::FileLock;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum CodexTrustError {
    #[error("Codex trust file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Codex trust file {path}: {message}")]
    Config { path: PathBuf, message: String },
}

/// Trust exactly one existing project root before starting Codex. The caller
/// must authorize it from live wt inventory or an explicitly managed root;
/// this function must never be fed arbitrary user-supplied directories.
///
/// This synchronous read/modify/write belongs on a blocking worker. It does
/// not read ambient HOME, trust siblings, or broaden trust to a parent folder.
pub fn ensure_trusted_project(
    config_path: &Path,
    project_root: &Path,
) -> Result<(), CodexTrustError> {
    let io_error = |path: &Path, source| CodexTrustError::Io {
        path: path.to_owned(),
        source,
    };
    let root = fs::canonicalize(project_root).map_err(|error| io_error(project_root, error))?;
    if !root.is_dir() || !root.join(".git").exists() {
        return Err(CodexTrustError::Config {
            path: config_path.into(),
            message: format!("{} is not an existing Git checkout", root.display()),
        });
    }
    let key = root.to_str().ok_or_else(|| CodexTrustError::Config {
        path: config_path.into(),
        message: "project path is not UTF-8".into(),
    })?;
    // Preserve a user-managed symlink rather than replacing it with a file.
    let path = match fs::symlink_metadata(config_path) {
        Ok(_) => fs::canonicalize(config_path).map_err(|error| io_error(config_path, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => config_path.to_owned(),
        Err(error) => return Err(io_error(config_path, error)),
    };
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".wt-lock");
    let _lock = FileLock::acquire(Path::new(&lock_path)).map_err(|error| io_error(&path, error))?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error(&path, error)),
    };
    let config_error = |message| CodexTrustError::Config {
        path: path.clone(),
        message,
    };
    let mut document = raw
        .parse::<DocumentMut>()
        .map_err(|error| config_error(format!("invalid TOML: {error}")))?;
    let projects = document
        .entry("projects")
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| config_error("projects is not a table".into()))?;
    if !projects.contains_key(key) {
        projects.insert(key, Item::Table(Table::new()));
    }
    let project = projects
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| config_error("project settings are not a table".into()))?;
    if project.get("trust_level").and_then(Item::as_str) == Some("trusted") {
        return Ok(());
    }
    let mut trusted = toml_edit::value("trusted");
    if let Some(existing) = project.get("trust_level").and_then(Item::as_value)
        && let Some(value) = trusted.as_value_mut()
    {
        *value.decor_mut() = existing.decor().clone();
    }
    project.insert("trust_level", trusted);
    // Codex doesn't share wt's lock. Refuse a concurrent edit observed before
    // replacement rather than overwriting it with the snapshot we read.
    match fs::read_to_string(&path) {
        Ok(current) if current == raw => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound && raw.is_empty() => {}
        Ok(_) => {
            return Err(config_error(
                "config changed while adding project trust; retry launch".into(),
            ));
        }
        Err(error) => return Err(io_error(&path, error)),
    }
    write_atomic(&path, document.to_string().as_bytes()).map_err(|error| io_error(&path, error))
}

fn write_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".wt-codex-trust-{}-{seq}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("rift checkout");
        fs::create_dir_all(root.join(".git")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let config = scratch.path().join("codex/config.toml");
        (scratch, root, config)
    }

    #[test]
    fn trusts_exact_root_and_preserves_comments_and_settings_idempotently() {
        let (_scratch, root, config) = fixture();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "# personal settings\nmodel = 'gpt-5' # retain\n[projects.'/unrelated']\ntrust_level = 'untrusted'\n[mcp_servers.docs]\ncommand = 'docs'\n").unwrap();
        ensure_trusted_project(&config, &root).unwrap();
        let saved = fs::read_to_string(&config).unwrap();
        let doc = saved.parse::<DocumentMut>().unwrap();
        assert_eq!(
            doc["projects"][root.to_str().unwrap()]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(
            doc["projects"]["/unrelated"]["trust_level"].as_str(),
            Some("untrusted")
        );
        assert!(
            doc["projects"]
                .as_table()
                .unwrap()
                .get(root.parent().unwrap().to_str().unwrap())
                .is_none()
        );
        assert_eq!(doc["mcp_servers"]["docs"]["command"].as_str(), Some("docs"));
        assert!(saved.contains("# personal settings"));
        assert!(saved.contains("# retain"));
        let modified = fs::metadata(&config).unwrap().modified().unwrap();
        ensure_trusted_project(&config, &root).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), saved);
        assert_eq!(fs::metadata(&config).unwrap().modified().unwrap(), modified);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn updates_only_trust_in_an_existing_inline_project() {
        let (_scratch, root, config) = fixture();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let mut document = DocumentMut::new();
        let mut project = toml_edit::InlineTable::new();
        project.insert("trust_level", "untrusted".into());
        project.insert("custom", 7.into());
        document["projects"][root.to_str().unwrap()] = toml_edit::value(project);
        fs::write(&config, document.to_string()).unwrap();
        ensure_trusted_project(&config, &root).unwrap();
        let saved = fs::read_to_string(&config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let project = &saved["projects"][root.to_str().unwrap()];
        assert_eq!(project["trust_level"].as_str(), Some("trusted"));
        assert_eq!(project["custom"].as_integer(), Some(7));
    }

    #[test]
    fn malformed_config_and_non_checkout_fail_without_replacement() {
        let (_scratch, root, config) = fixture();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        for malformed in ["broken = [", "projects = 42"] {
            fs::write(&config, malformed).unwrap();
            assert!(ensure_trusted_project(&config, &root).is_err());
            assert_eq!(fs::read_to_string(&config).unwrap(), malformed);
        }
        fs::write(&config, "model = 'gpt-5'\n").unwrap();
        assert!(ensure_trusted_project(&config, root.parent().unwrap()).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "model = 'gpt-5'\n");
    }

    #[cfg(unix)]
    #[test]
    fn config_and_checkout_symlinks_preserve_the_link_and_trust_canonical_root() {
        let (scratch, root, config) = fixture();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let target = scratch.path().join("settings.toml");
        fs::write(&target, "model = 'gpt-5'\n").unwrap();
        std::os::unix::fs::symlink(&target, &config).unwrap();
        let alias = scratch.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        ensure_trusted_project(&config, &alias).unwrap();
        assert!(
            fs::symlink_metadata(&config)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let saved = fs::read_to_string(&target)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            saved["projects"][root.to_str().unwrap()]["trust_level"].as_str(),
            Some("trusted")
        );
        assert!(
            saved["projects"]
                .as_table()
                .unwrap()
                .get(alias.to_str().unwrap())
                .is_none()
        );
    }

    #[test]
    fn concurrent_project_additions_preserve_both_entries() {
        let (scratch, root, config) = fixture();
        let second = scratch.path().join("second");
        fs::create_dir_all(second.join(".git")).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| ensure_trusted_project(&config, &root).unwrap());
            scope.spawn(|| ensure_trusted_project(&config, &second).unwrap());
        });
        let saved = fs::read_to_string(&config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(saved["projects"].as_table().unwrap().len(), 2);
    }
}
