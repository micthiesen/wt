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
    store_for_build(config, crate::build_info::build_id(), executable_stamp())
}

/// Remote display snapshots survive compatible binary updates. Their schema
/// and selected endpoint, rather than the executable, define compatibility.
/// Bump the cache schema when a display field changes meaning incompatibly.
pub fn remote_store(config: &Config, endpoint: &wt_config::RemoteConfig) -> CacheStore {
    let mut hash = Sha256::new();
    for identity in [
        "remote-display-v1".to_owned(),
        crate::host_protocol::HOST_PROTOCOL.to_string(),
        config.repo_id.clone(),
        std::fs::canonicalize(&config.paths.main_clone)
            .unwrap_or_else(|_| config.paths.main_clone.clone())
            .to_string_lossy()
            .into_owned(),
        format!("{:?}", config.repository_config),
        format!(
            "{:?}",
            (
                &endpoint.host,
                &endpoint.config,
                endpoint.managed,
                &endpoint.managed_root
            )
        ),
    ] {
        hash.update(identity.as_bytes());
        hash.update([0]);
    }
    CacheStore::new(
        config.paths.cache_root.join("remote-query"),
        format!("{:x}", hash.finalize()),
    )
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
    async fn remote_scope_uses_endpoint_and_schema_instead_of_local_display_settings() {
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let mut config = (*fixture.ctx.config).clone();
        let endpoint = wt_config::RemoteConfig {
            host: "builder".into(),
            ..Default::default()
        };
        let scope = remote_store(&config, &endpoint).scope().to_owned();
        assert_ne!(
            store_for_build(&config, "old-build", None).scope(),
            store_for_build(&config, "new-build", None).scope()
        );
        config.branch.base = "develop".into();
        config.harness.primary = wt_core::HarnessId::Opencode;
        assert_eq!(remote_store(&config, &endpoint).scope(), scope);
        let mut relabeled = endpoint.clone();
        relabeled.label = "New label".into();
        assert_eq!(remote_store(&config, &relabeled).scope(), scope);
        let mut other = endpoint.clone();
        other.config = Some("~/other/config.toml".into());
        assert_ne!(remote_store(&config, &other).scope(), scope);
        other = endpoint.clone();
        other.host = "another-host".into();
        assert_ne!(remote_store(&config, &other).scope(), scope);
        config.repo_id = "another-repository".into();
        assert_ne!(remote_store(&config, &endpoint).scope(), scope);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn scope_changes_with_configuration_build_and_dev_binary() {
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
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
