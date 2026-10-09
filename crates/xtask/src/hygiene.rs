use std::{
    fs::{self, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};

const DEFAULT_LIMIT_GIB: u64 = 10;
const GIB: u64 = 1 << 30;

/// Report Cargo target size without deleting artifacts or test evidence.
pub fn report(root: &Path) -> Result<()> {
    let mut roots = vec![root.join("target")];
    if let Some(configured) = std::env::var_os("CARGO_TARGET_DIR") {
        let configured = PathBuf::from(configured);
        let configured = if configured.is_absolute() {
            configured
        } else {
            root.join(configured)
        };
        if !roots.contains(&configured) {
            roots.push(configured);
        }
    }
    let bytes = roots.iter().try_fold(0u64, |sum, root| {
        Ok::<_, anyhow::Error>(sum.saturating_add(tree_bytes(root)?))
    })?;
    let limit = target_limit_bytes();
    if bytes > limit {
        println!(
            "hygiene: Cargo targets use {} GiB (soft limit {} GiB); no files removed",
            bytes / GIB,
            limit / GIB
        );
    } else {
        println!(
            "hygiene: Cargo targets use {} GiB (soft limit {} GiB)",
            bytes / GIB,
            limit / GIB
        );
    }
    Ok(())
}

/// Remove old incremental compiler state only after exclusively locking every
/// target profile the command might touch. No compiled outputs or test results
/// are deleted. This is explicit and never runs as part of `gate`.
pub fn clean_incremental(root: &Path) -> Result<()> {
    let target = root.join("target");
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && metadata.file_type().is_symlink()
    {
        anyhow::bail!(
            "refusing cleanup because workspace target {} is a symlink",
            target.display()
        );
    }
    let roots = vec![target];
    let profiles = profile_dirs(&roots)?;
    let mut locks = Vec::new();
    for profile in &profiles {
        let path = profile.join(".cargo-lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening Cargo build lock {}", path.display()))?;
        if let Err(error) = lock.try_lock() {
            match error {
                TryLockError::WouldBlock => anyhow::bail!(
                    "Cargo is using {}; incremental cleanup refused",
                    profile.display()
                ),
                TryLockError::Error(error) => {
                    return Err(error)
                        .with_context(|| format!("locking Cargo profile {}", profile.display()));
                }
            }
        }
        locks.push(lock);
    }

    let before = roots.iter().try_fold(0u64, |sum, root| {
        Ok::<_, anyhow::Error>(sum.saturating_add(tree_bytes(root)?))
    })?;
    let limit = target_limit_bytes();
    if before <= limit {
        println!("hygiene: targets are below the limit; no cleanup needed");
        return Ok(());
    }
    let removed = prune_incremental(&profiles, &roots, limit)?;
    drop(locks);
    let after = roots.iter().try_fold(0u64, |sum, root| {
        Ok::<_, anyhow::Error>(sum.saturating_add(tree_bytes(root)?))
    })?;
    println!(
        "hygiene: removed {removed} old incremental cache directories; targets {} -> {} GiB; compiled outputs and test evidence were preserved",
        before / GIB,
        after / GIB
    );
    if after > limit {
        println!(
            "hygiene: remaining target contents exceed the limit; no build outputs or evidence were removed"
        );
    }
    Ok(())
}

fn target_limit_bytes() -> u64 {
    std::env::var("WT_TARGET_LIMIT_GIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LIMIT_GIB)
        .saturating_mul(GIB)
}

fn prune_incremental(profiles: &[PathBuf], roots: &[PathBuf], limit: u64) -> Result<usize> {
    let mut sessions = Vec::new();
    for profile in profiles {
        let incremental = profile.join("incremental");
        let Ok(entries) = fs::read_dir(&incremental) else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", incremental.display()))?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let modified = entry
                .metadata()?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            sessions.push((modified, entry.path()));
        }
    }
    sessions.sort_by_key(|(modified, _)| *modified);
    let mut total = roots.iter().try_fold(0u64, |sum, root| {
        Ok::<_, anyhow::Error>(sum.saturating_add(tree_bytes(root)?))
    })?;
    let mut removed = 0;
    for (_, path) in sessions {
        if total <= limit {
            break;
        }
        let bytes = tree_bytes(&path)?;
        fs::remove_dir_all(&path)
            .with_context(|| format!("removing incremental cache {}", path.display()))?;
        total = total.saturating_sub(bytes);
        removed += 1;
    }
    Ok(removed)
}

fn profile_dirs(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut profiles = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let mut pending = vec![(root.clone(), 0usize)];
        while let Some((directory, depth)) = pending.pop() {
            if directory.join(".fingerprint").is_dir() {
                profiles.push(directory);
                continue;
            }
            if depth >= 4 {
                continue;
            }
            for entry in fs::read_dir(&directory)
                .with_context(|| format!("reading {}", directory.display()))?
            {
                let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
                if entry.file_type()?.is_dir() {
                    pending.push((entry.path(), depth + 1));
                }
            }
        }
    }
    profiles.sort();
    profiles.dedup();
    Ok(profiles)
}

fn tree_bytes(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
            let kind = entry
                .file_type()
                .with_context(|| format!("inspecting {}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_prunes_only_incremental_state_and_stops_at_the_limit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("target");
        let profile = target.join("debug");
        fs::create_dir_all(profile.join(".fingerprint"))?;
        fs::create_dir_all(profile.join("incremental/old"))?;
        fs::write(profile.join("incremental/old/cache"), vec![0; 10])?;
        fs::write(profile.join("evidence.txt"), vec![0; 10])?;

        let removed = prune_incremental(
            std::slice::from_ref(&profile),
            std::slice::from_ref(&target),
            10,
        )?;

        assert_eq!(removed, 1);
        assert!(!profile.join("incremental/old").exists());
        assert_eq!(fs::metadata(profile.join("evidence.txt"))?.len(), 10);
        assert_eq!(tree_bytes(&target)?, 10);
        Ok(())
    }

    #[test]
    fn workspace_target_symlink_is_rejected_before_deletion() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, temp.path().join("target"))?;

        #[cfg(unix)]
        assert!(clean_incremental(temp.path()).is_err());
        Ok(())
    }
}
