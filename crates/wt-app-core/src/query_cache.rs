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
    let main_clone = std::fs::canonicalize(&config.paths.main_clone)
        .unwrap_or_else(|_| config.paths.main_clone.clone());
    let mut parts = vec![
        ("build", fingerprint(build)),
        ("main-clone", fingerprint(&main_clone.to_string_lossy())),
    ];
    // Development builds share one build id across rebuilds; the binary's
    // size and modification time tell them apart.
    if build.starts_with("dev-")
        && let Some(executable) = executable
    {
        parts.push(("binary", fingerprint(&executable)));
    }
    parts.extend(config_parts(config));
    CacheStore::from_parts(config.paths.cache_root.join("query"), parts)
}

/// One component per configuration section a source's data can depend on.
/// The destructuring is exhaustive so a new section has to be placed here.
fn config_parts(config: &Config) -> Vec<(&'static str, String)> {
    let Config {
        repository_config,
        instance,
        repo_id,
        repo_path,
        paths,
        tmux,
        resources,
        branch,
        environments,
        history,
        stage,
        lifecycle,
        backend,
        sst,
        issue_tracker,
        review_bot,
        dev_server,
        remotes,
        managed,
        harness,
        github,
        // What follows decides how the board is drawn or what wt does with
        // it, never what a source read: editing one keeps the cache.
        checks: _,
        notify: _,
        diff: _,
        editor: _,
        browser: _,
        guides: _,
        actions: _,
        automations: _,
        mcp_servers: _,
        ui: _,
        skills: _,
        manager: _,
        update: _,
        maintenance: _,
        warnings: _,
    } = config;
    // No section holds a hash map, so each debug form is stable across
    // processes.
    macro_rules! parts {
        ($($name:literal => $value:expr),* $(,)?) => {
            vec![$(($name, fingerprint(&format!("{:?}", $value)))),*]
        };
    }
    parts![
        "repository" => (repository_config, repo_id, repo_path),
        "instance" => instance,
        "paths" => paths,
        "tmux" => tmux,
        "resources" => resources,
        "branch" => branch,
        "environments" => environments,
        "history" => history,
        "stage" => stage,
        "lifecycle" => lifecycle,
        "backend" => backend,
        "sst" => sst,
        "issue_tracker" => issue_tracker,
        "review_bot" => review_bot,
        "dev_server" => dev_server,
        "remotes" => remotes,
        "managed" => managed,
        "harness" => harness,
        "github" => github,
    ]
}

fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

    #[test]
    fn scope_survives_a_reload_and_names_the_section_that_changed() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let repo = root.path().join("main");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(repo.join("nested")).unwrap();
        let write = |extra: &str| {
            std::fs::write(
                repo.join(".wt.toml"),
                format!(
                    "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n[branch]\nprefix = \"fx\"\n{extra}",
                    repo.to_string_lossy(),
                    root.path().join("worktrees").to_string_lossy(),
                ),
            )
            .unwrap();
        };
        // A different working directory and environment each time, as two
        // launches of one installation have.
        let load = |cwd: &Path, variable: &str| {
            let env = [(variable.to_owned(), "1".to_owned())].into();
            let options = wt_config::LoadOptions::new(cwd, &home, env);
            store_for_build(&Config::load(&options).unwrap(), "v1", None)
        };
        write("base = \"main\"\n");
        let first = load(&repo, "TERM_SESSION");
        assert_eq!(
            first.scope(),
            load(&repo.join("nested"), "TMUX_PANE").scope()
        );

        // A presentation setting leaves restored data valid.
        write("base = \"main\"\n[ui]\nowed_sections = true\n");
        assert_eq!(first.scope(), load(&repo, "TERM_SESSION").scope());

        write("base = \"develop\"\n");
        let rebased = load(&repo, "TERM_SESSION");
        assert_eq!(
            wt_runtime::scope_difference(first.scope(), rebased.scope()),
            "branch"
        );
        assert_eq!(
            wt_runtime::scope_difference(
                first.scope(),
                store_for_build(
                    &Config::load(&wt_config::LoadOptions::new(
                        &repo,
                        &home,
                        Default::default()
                    ))
                    .unwrap(),
                    "v2",
                    None
                )
                .scope()
            ),
            "branch,build"
        );
    }
}
