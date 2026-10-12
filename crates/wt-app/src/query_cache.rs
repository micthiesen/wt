//! The disposable source cache's location and identity. Restored data is only
//! shown, never confirmed, but it must still describe this repository and
//! configuration, so the scope changes whenever either could have changed.
use std::path::Path;

use sha2::{Digest, Sha256};
use wt_config::Config;
use wt_runtime::CacheStore;

/// One store per configuration; its directory is inside the repository's
/// cache root, and its scope fingerprints everything restored data depends on.
pub fn store(config: &Config) -> CacheStore {
    store_for_build(config, env!("WT_BUILD_ID"), executable_stamp())
}

fn store_for_build(config: &Config, build: &str, executable: Option<String>) -> CacheStore {
    let mut hash = Sha256::new();
    hash.update(config.repo_id.as_bytes());
    hash.update([0]);
    let main_clone = std::fs::canonicalize(&config.paths.main_clone)
        .unwrap_or_else(|_| config.paths.main_clone.clone());
    hash.update(main_clone.to_string_lossy().as_bytes());
    hash.update([0]);
    // The merged configuration has no hash maps, so its debug form is stable.
    hash.update(format!("{config:?}").as_bytes());
    hash.update([0]);
    hash.update(build.as_bytes());
    hash.update([0]);
    // Development builds share one build id across rebuilds; the binary's
    // size and modification time tell them apart.
    if build.starts_with("dev-")
        && let Some(executable) = executable
    {
        hash.update(executable.as_bytes());
    }
    CacheStore::new(
        config.paths.cache_root.join("query"),
        format!("{:x}", hash.finalize()),
    )
}

fn executable_stamp() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    stamp(&path)
}

fn stamp(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}:{modified}", metadata.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scope_changes_with_configuration_build_and_dev_binary() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let config = (*fixture.ctx.config).clone();
        let base = store_for_build(&config, "v1", None);
        assert_eq!(base.scope(), store_for_build(&config, "v1", None).scope());
        assert_ne!(base.scope(), store_for_build(&config, "v2", None).scope());
        let mut other = config.clone();
        other.repo_id = "another".into();
        assert_ne!(base.scope(), store_for_build(&other, "v1", None).scope());
        let mut changed = config.clone();
        changed.branch.base = "develop".into();
        assert_ne!(base.scope(), store_for_build(&changed, "v1", None).scope());
        // Release builds ignore the binary stamp; dev builds use it.
        assert_eq!(
            base.scope(),
            store_for_build(&config, "v1", Some("1:2".into())).scope()
        );
        assert_ne!(
            store_for_build(&config, "dev-1", Some("1:2".into())).scope(),
            store_for_build(&config, "dev-1", Some("1:3".into())).scope()
        );
        assert!(base.directory().starts_with(&config.paths.cache_root));
        fixture.close().await.unwrap();
    }
}
