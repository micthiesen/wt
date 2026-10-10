//! Config-free native installation and explicit legacy path migration.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use wt_config::LoadOptions;
use wt_update::{Channel, InstallOutcome, InstallPaths, ReleaseSource, StateStore};
const NETWORK_TIMEOUT: Duration = Duration::from_secs(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallResult {
    Installed { release: String, build_id: String },
    AlreadyCurrent { release: String, build_id: String },
}

pub fn install_paths(options: &LoadOptions) -> Result<InstallPaths> {
    let root = options
        .env
        .get(wt_launcher::INSTALL_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| options.home.join(".local/share/wt"));
    InstallPaths::new(root).context("resolve native wt installation root")
}

/// Fetch and install a release without loading repository config or touching a
/// checkout. The exact-tag mode is used by the shell bootstrap and test assets.
pub async fn install_once(
    options: &LoadOptions,
    channel: Option<Channel>,
    release_tag: Option<String>,
    expected_build_id: Option<String>,
    create_path_link: bool,
    cancel: &CancellationToken,
) -> Result<InstallResult> {
    let paths = install_paths(options)?;
    let repository = crate::updates::repository(options)?;
    let store = StateStore::new(paths.clone());
    let state = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.load()
    })
    .await
    .context("join installer state read")??;
    let selected_channel = channel.unwrap_or(state.channel);
    let source = ReleaseSource::new(repository)?;
    let release = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("installation cancelled before release selection"),
        result = tokio::time::timeout(NETWORK_TIMEOUT, async {
            match release_tag.as_deref() {
                Some(tag) => source.by_tag(tag).await,
                None => source.latest(selected_channel).await,
            }
        }) => result.context("release metadata request timed out")??,
    };
    let target = env!("WT_TARGET");
    let version = release.version_id(target)?;
    if let Some(expected) = expected_build_id
        && version.build_id() != expected
    {
        bail!(
            "bootstrap binary build {} does not match selected release build {}; run install from the exact release artifact",
            expected,
            version.build_id()
        )
    }
    let verified = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("installation cancelled before release download"),
        result = tokio::time::timeout(NETWORK_TIMEOUT, source.download_verified(&release, target)) => result.context("release archive download timed out")??,
    };
    let outcome = wt_update::install_verified(
        paths.clone(),
        verified,
        crate::updates::attempt_token(),
        now_unix(),
    )
    .await
    .context("verify and activate native release")?;

    if let Some(channel) = channel {
        let at = now_unix();
        let check_paths = paths.clone();
        tokio::task::spawn_blocking(move || {
            wt_update::InstallManager::new(check_paths).record_check(channel, at)
        })
        .await
        .context("join installer channel update")??;
    }

    let link_result = migrate_or_link_path(&paths, &options.home, create_path_link)
        .context("install or migrate wt PATH entry");
    if let Err(error) = link_result {
        bail!(
            "native release {} ({}) is installed at {}, but PATH migration failed: {error:#}",
            version.release_version(),
            version.build_id(),
            paths.root().display()
        )
    }

    Ok(match outcome {
        InstallOutcome::Installed => InstallResult::Installed {
            release: version.release_version().to_owned(),
            build_id: version.build_id().to_owned(),
        },
        InstallOutcome::AlreadyCurrent => InstallResult::AlreadyCurrent {
            release: version.release_version().to_owned(),
            build_id: version.build_id().to_owned(),
        },
    })
}

pub fn migrate_or_link_path(
    paths: &InstallPaths,
    home: &Path,
    create_if_missing: bool,
) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (paths, home, create_if_missing);
        bail!("native PATH link management currently supports macOS and Linux")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let stable = paths.launcher();
        let link = home.join(".local/bin/wt");
        match fs::symlink_metadata(&link) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !create_if_missing {
                    return Ok(());
                }
                let parent = link.parent().expect("PATH entry has parent");
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
                let temp = parent.join(format!(".wt-link-{}.tmp", unique_id()));
                symlink(&stable, &temp)
                    .with_context(|| format!("create temporary symlink {}", temp.display()))?;
                if let Err(error) = fs::rename(&temp, &link) {
                    let _ = fs::remove_file(&temp);
                    return Err(error)
                        .with_context(|| format!("activate PATH symlink {}", link.display()));
                }
                sync_directory(parent)?;
                Ok(())
            }
            Err(error) => {
                Err(error).with_context(|| format!("inspect PATH entry {}", link.display()))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if canonical_target(&link)
                    .ok()
                    .zip(canonical_target(&stable).ok())
                    .is_some_and(|(link, stable)| link == stable)
                {
                    return Ok(());
                }
                if !is_recognized_legacy_link(&link, home)? {
                    if create_if_missing {
                        bail!(
                            "refusing to replace unrelated PATH entry {}",
                            link.display()
                        )
                    }
                    return Ok(());
                }
                backup_legacy_checkout(paths, home, &link)?;
                let parent = link.parent().expect("PATH entry has parent");
                let temp = parent.join(format!(".wt-link-{}.tmp", unique_id()));
                symlink(&stable, &temp).with_context(|| {
                    format!("create replacement PATH symlink {}", temp.display())
                })?;
                if let Err(error) = fs::rename(&temp, &link) {
                    let _ = fs::remove_file(&temp);
                    return Err(error).with_context(|| {
                        format!("activate migrated PATH symlink {}", link.display())
                    });
                }
                sync_directory(parent)?;
                Ok(())
            }
            Ok(_) if create_if_missing => {
                bail!("refusing to replace unrelated PATH file {}", link.display())
            }
            Ok(_) => Ok(()),
        }
    }
}

#[cfg(unix)]
fn is_recognized_legacy_link(link: &Path, home: &Path) -> Result<bool> {
    let checkout = home.join(".wt");
    let shim = checkout.join("bin/wt");
    if !checkout.join("src/main.ts").is_file()
        || !checkout.join("package.json").is_file()
        || !checkout.join(".git").exists()
        || !shim.is_file()
    {
        return Ok(false);
    }
    let contents =
        fs::read(&shim).with_context(|| format!("read legacy shim {}", shim.display()))?;
    if !contents.starts_with(b"#!/bin/sh") || !contents.windows(3).any(|window| window == b"bun") {
        return Ok(false);
    }
    Ok(canonical_target(link)
        .ok()
        .zip(canonical_target(&shim).ok())
        .is_some_and(|(link, shim)| link == shim))
}

#[cfg(unix)]
fn backup_legacy_checkout(paths: &InstallPaths, home: &Path, link: &Path) -> Result<()> {
    let checkout = home.join(".wt");
    let root = paths.root();
    if checkout.starts_with(root) || root.starts_with(&checkout) {
        bail!("install root and legacy checkout overlap; refusing recursive migration backup")
    }
    let dir = root.join("migrations");
    fs::create_dir_all(&dir)
        .with_context(|| format!("create migration backup directory {}", dir.display()))?;
    let id = unique_id();
    let archive = dir.join(format!("legacy-checkout-{id}.tar.gz"));
    let output = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&checkout)
        .arg(".")
        .output()
        .context("archive the recognized legacy source checkout")?;
    if !output.status.success() {
        let _ = fs::remove_file(&archive);
        bail!(
            "tar could not back up legacy checkout {}: {}",
            checkout.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    File::open(&archive)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync legacy checkout backup {}", archive.display()))?;
    let shim = checkout.join("bin/wt");
    let shim_bytes =
        fs::read(&shim).with_context(|| format!("read legacy shim {}", shim.display()))?;
    let symlink_target =
        fs::read_link(link).with_context(|| format!("read legacy PATH link {}", link.display()))?;
    let manifest = MigrationBackup {
        schema_version: 1,
        checkout: checkout.to_string_lossy().into_owned(),
        checkout_archive: archive.to_string_lossy().into_owned(),
        legacy_shim_sha256: hex_sha256(&shim_bytes),
        previous_path_target: symlink_target.to_string_lossy().into_owned(),
        checkout_left_in_place: true,
    };
    let manifest_path = dir.join(format!("legacy-migration-{id}.json"));
    let bytes =
        serde_json::to_vec_pretty(&manifest).context("serialize migration backup record")?;
    wt_update::atomic_write(&manifest_path, &bytes)
        .with_context(|| format!("write migration backup record {}", manifest_path.display()))?;
    sync_directory(&dir)?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationBackup {
    schema_version: u32,
    checkout: String,
    checkout_archive: String,
    legacy_shim_sha256: String,
    previous_path_target: String,
    checkout_left_in_place: bool,
}

fn canonical_target(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn legacy_tree(home: &Path) -> (PathBuf, PathBuf) {
        let checkout = home.join(".wt");
        fs::create_dir_all(checkout.join("bin")).unwrap();
        fs::create_dir_all(checkout.join("src")).unwrap();
        fs::write(checkout.join("src/main.ts"), "// old source").unwrap();
        fs::write(checkout.join("package.json"), "{}").unwrap();
        fs::write(
            checkout.join("user-settings.toml"),
            "[private]\nvalue = \"kept\"\n",
        )
        .unwrap();
        fs::create_dir(checkout.join(".git")).unwrap();
        let shim = checkout.join("bin/wt");
        fs::write(&shim, "#!/bin/sh\nexec bun src/main.ts \"$@\"\n").unwrap();
        let link = home.join(".local/bin/wt");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&shim, &link).unwrap();
        (checkout, link)
    }

    #[test]
    fn path_link_migration_archives_legacy_checkout_and_keeps_source_in_place() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let (checkout, link) = legacy_tree(&home);
        let paths = InstallPaths::new(home.join(".local/share/wt")).unwrap();
        fs::create_dir_all(paths.launcher().parent().unwrap()).unwrap();
        fs::write(paths.launcher(), b"stable launcher").unwrap();
        migrate_or_link_path(&paths, &home, false).unwrap();
        assert_eq!(
            canonical_target(&link).unwrap(),
            canonical_target(&paths.launcher()).unwrap()
        );
        assert!(checkout.join("src/main.ts").is_file());
        let migrations = fs::read_dir(paths.root().join("migrations")).unwrap();
        let files = migrations
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert!(
            files
                .iter()
                .any(|path| path.extension().is_some_and(|ext| ext == "gz"))
        );
        let archive = files
            .iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "gz"))
            .unwrap();
        let listing = Command::new("tar")
            .arg("-tzf")
            .arg(archive)
            .output()
            .unwrap();
        assert!(listing.status.success());
        assert!(String::from_utf8_lossy(&listing.stdout).contains("user-settings.toml"));
        let manifest = files
            .iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
        assert_eq!(value["checkoutLeftInPlace"], true);
    }

    #[test]
    fn path_link_creation_never_replaces_unowned_file_or_link() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let paths = InstallPaths::new(home.join(".local/share/wt")).unwrap();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        fs::write(home.join(".local/bin/wt"), "user executable").unwrap();
        assert!(migrate_or_link_path(&paths, &home, true).is_err());
        assert_eq!(
            fs::read(home.join(".local/bin/wt")).unwrap(),
            b"user executable"
        );
    }

    #[test]
    fn unrelated_symlink_is_left_alone_without_explicit_path_link_and_rejected_with_it() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        let other = temp.path().join("other-wt");
        fs::write(&other, "other").unwrap();
        std::os::unix::fs::symlink(&other, home.join(".local/bin/wt")).unwrap();
        let paths = InstallPaths::new(home.join(".local/share/wt")).unwrap();
        migrate_or_link_path(&paths, &home, false).unwrap();
        assert!(migrate_or_link_path(&paths, &home, true).is_err());
        assert_eq!(fs::read(&other).unwrap(), b"other");
    }

    #[test]
    fn legacy_backup_refuses_overlapping_install_root() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let (_, link) = legacy_tree(&home);
        let paths = InstallPaths::new(home.join(".wt/native")).unwrap();
        assert!(backup_legacy_checkout(&paths, &home, &link).is_err());
    }
}
