use std::collections::BTreeMap;

use wt_config::{Config, LoadOptions, memory_bytes};

fn load(extra: &str) -> Result<Config, wt_config::ConfigError> {
    let directory = tempfile::tempdir().unwrap();
    let options = LoadOptions::new(directory.path(), directory.path(), BTreeMap::new());
    let raw = format!(
        "[paths]\nmain_clone = '/fixture/repo'\nworktree_root = '/fixture/trees'\n[branch]\nprefix = 'test'\n{extra}"
    );
    Config::from_raw(toml::from_str(&raw).unwrap(), &options)
}

#[test]
fn limits_are_opt_in_and_reject_unsafe_or_ineffective_values() {
    assert!(!load("").unwrap().resources.linux.active());
    for settings in [
        "[resources.linux.job]\ncpu_quota_percent = 200",
        "[resources.linux]\ngroup = '../other'\n[resources.linux.job]\ncpu_quota_percent = 200",
        "[resources.linux]\ngroup = 'test'\n[resources.linux.job]\ncpu_quota_percent = 0",
        "[resources.linux]\ngroup = 'test'\n[resources.linux.job]\nmemory_max = '0'",
        "[resources.linux]\ngroup = 'test'\n[resources.linux.job]\nmemory_max = '4G; touch /tmp/injected'",
        "[resources.linux]\ngroup = 'test'\n[resources.linux.job]\nmemory_max = '18446744073709551615T'",
        "[resources.linux]\ngroup = 'test'\n[resources.linux.job]\nmemory_high = '8G'\nmemory_max = '4G'",
        "[resources.linux.job]\nmisspelled_quota = 200",
        "[resources]\nlinux = false",
    ] {
        assert!(load(settings).is_err(), "accepted: {settings}");
    }
    let config = load("[resources.linux]\ngroup = 'test'\n[resources.linux.job]\nmemory_max = '4G'\nmemory_swap_max = '0'").unwrap();
    assert!(config.resources.linux.active());
    assert_eq!(memory_bytes("4G"), Some(4 * 1024 * 1024 * 1024));
}

#[test]
fn repository_overrides_merge_budgets_and_can_disable_inherited_limits() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let repo = directory.path().join("repo");
    std::fs::create_dir_all(home.join(".config/wt")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let user = home.join(".config/wt/config.toml");
    std::fs::write(&user, format!(
        "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n[branch]\nprefix = 'test'\n[resources.linux]\ngroup = 'cozee'\n[resources.linux.shared]\ncpu_quota_percent = 600\nmemory_max = '12G'\n[resources.linux.job]\ncpu_quota_percent = 200\n",
        repo, directory.path().join("trees")
    )).unwrap();
    let selected = repo.join(".wt.toml");
    std::fs::write(
        &selected,
        "[resources.linux.job]\ncpu_quota_percent = 150\n",
    )
    .unwrap();
    let options = LoadOptions::new(&repo, &home, BTreeMap::new());
    let config = Config::load(&options).unwrap();
    assert_eq!(
        config.resources.linux.shared.memory_max.as_deref(),
        Some("12G")
    );
    assert_eq!(config.resources.linux.shared.cpu_quota_percent, Some(600));
    assert_eq!(config.resources.linux.job.cpu_quota_percent, Some(150));
    std::fs::write(&selected, "[resources.linux]\nenabled = false\n").unwrap();
    assert!(!Config::load(&options).unwrap().resources.linux.active());
    assert!(
        config.resources.linux.active(),
        "the earlier process keeps its selected settings"
    );
}

#[test]
fn managed_host_overrides_preserve_defaults_and_other_hosts() {
    let config = load("[resources.linux]\ngroup = 'jobs'\n[resources.linux.shared]\ncpu_quota_percent = 600\nmemory_max = '12G'\n[[remotes]]\nhost = 'small'\nmanaged = true\n[remotes.resources.linux.shared]\ncpu_quota_percent = 200\n[[remotes]]\nhost = 'large'\nmanaged = true\n").unwrap();
    let small = config.remotes[0].resources.as_ref().unwrap();
    assert_eq!(small.linux.shared.cpu_quota_percent, Some(200));
    assert_eq!(small.linux.shared.memory_max.as_deref(), Some("12G"));
    assert_eq!(small.linux.group, "jobs");
    assert!(config.remotes[1].resources.is_none());
    assert_eq!(config.resources.linux.shared.cpu_quota_percent, Some(600));
    assert!(
        load("[remote]\nhost = 'unmanaged'\n[remote.resources.linux]\nenabled = false").is_err()
    );
    assert!(load("[remote]\nhost = 'managed'\nmanaged = true\n[remote.resources.linux.job]\ncpu_quota_percent = 0").is_err());
}
