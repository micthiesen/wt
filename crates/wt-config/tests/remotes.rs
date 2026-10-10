use std::{collections::BTreeMap, fs, path::Path};

use wt_config::{Config, LoadOptions};

fn options(cwd: &Path, home: &Path, env: &[(&str, &str)]) -> LoadOptions {
    LoadOptions::new(
        cwd,
        home,
        env.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn minimal_config(extra: &str) -> String {
    format!(
        "[paths]\nmain_clone = '/repo'\nworktree_root = '/repo/worktrees'\n\n[branch]\nprefix = 'm'\n{extra}"
    )
}

#[test]
fn a_process_loads_its_selected_remote_config_without_mutating_another_selection() {
    let temp = tempfile::tempdir().unwrap();
    let first_path = temp.path().join("first.toml");
    let second_path = temp.path().join("second.toml");
    fs::write(
        &first_path,
        minimal_config("\n[remote]\nhost = 'buildbox'\nconfig = '/etc/wt/first.toml'\n"),
    )
    .unwrap();
    fs::write(
        &second_path,
        minimal_config("\n[remote]\nhost = 'buildbox'\nconfig = '/etc/wt/second.toml'\n"),
    )
    .unwrap();

    let first = Config::load(&options(
        temp.path(),
        temp.path(),
        &[("WT_CONFIG", first_path.to_str().unwrap())],
    ))
    .unwrap();
    let second = Config::load(&options(
        temp.path(),
        temp.path(),
        &[("WT_CONFIG", second_path.to_str().unwrap())],
    ))
    .unwrap();

    assert_eq!(first.remotes[0].host, "buildbox");
    assert_eq!(second.remotes[0].host, "buildbox");
    assert_eq!(
        first.remotes[0].config.as_deref(),
        Some("/etc/wt/first.toml")
    );
    assert_eq!(
        second.remotes[0].config.as_deref(),
        Some("/etc/wt/second.toml")
    );
    assert_ne!(first.remotes[0].key(), second.remotes[0].key());
}

#[test]
fn legacy_remote_key_and_defaults_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let raw = toml::from_str(&minimal_config("\n[remote]\nhost = 'worker-alias'\n")).unwrap();
    let config = Config::from_raw(raw, &options(root.path(), root.path(), &[])).unwrap();

    let remote = &config.remotes[0];
    assert_eq!(remote.host, "worker-alias");
    assert_eq!(remote.label, "worker-alias");
    assert_eq!(remote.wt_path, "~/.wt/bin/wt");
    assert_eq!(remote.config, None);
    assert_eq!(remote.key(), "worker-alias");
}

#[test]
fn mixed_legacy_and_plural_remote_tables_are_loaded_in_order() {
    let root = tempfile::tempdir().unwrap();
    let raw = toml::from_str(&minimal_config(
        "\n[remote]\nhost = 'legacy'\n\n[[remotes]]\nhost = 'one'\nconfig = '~/one.toml'\n\n[[remotes]]\nhost = 'two'\nconfig = '/etc/wt/two.toml'\n",
    ))
    .unwrap();
    let config = Config::from_raw(raw, &options(root.path(), root.path(), &[])).unwrap();

    assert_eq!(
        config
            .remotes
            .iter()
            .map(|remote| remote.host.as_str())
            .collect::<Vec<_>>(),
        ["legacy", "one", "two"]
    );
    assert_eq!(config.remotes[1].config.as_deref(), Some("~/one.toml"));
}

#[test]
fn duplicate_ssh_destinations_are_rejected_even_when_config_differs() {
    let root = tempfile::tempdir().unwrap();
    let raw = toml::from_str(&minimal_config(
        "\n[[remotes]]\nhost = 'worker'\nconfig = '/one.toml'\n\n[[remotes]]\nhost = 'worker'\nconfig = '/two.toml'\n",
    ))
    .unwrap();
    let error = Config::from_raw(raw, &options(root.path(), root.path(), &[])).unwrap_err();

    assert!(error.to_string().contains("duplicates SSH host"));
}

#[test]
fn option_like_or_whitespace_hosts_and_relative_or_control_paths_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    for (host, config) in [
        ("-oProxyCommand=bad", "/etc/wt.toml"),
        ("user@host name", "/etc/wt.toml"),
        ("worker", "relative.toml"),
        ("worker", "/etc/wt/\u{7f}bad.toml"),
    ] {
        let raw = toml::Value::Table({
            let mut table = toml::map::Map::new();
            table.insert(
                "paths".into(),
                toml::Value::Table(toml::map::Map::from_iter([
                    ("main_clone".into(), toml::Value::String("/repo".into())),
                    (
                        "worktree_root".into(),
                        toml::Value::String("/repo/worktrees".into()),
                    ),
                ])),
            );
            table.insert(
                "branch".into(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "prefix".into(),
                    toml::Value::String("m".into()),
                )])),
            );
            table.insert(
                "remote".into(),
                toml::Value::Table(toml::map::Map::from_iter([
                    ("host".into(), toml::Value::String(host.into())),
                    ("config".into(), toml::Value::String(config.into())),
                ])),
            );
            table
        });
        assert!(
            Config::from_raw(raw, &options(root.path(), root.path(), &[])).is_err(),
            "accepted host={host:?} config={config:?}"
        );
    }
}
