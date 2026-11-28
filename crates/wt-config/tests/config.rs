use std::fs;
use std::path::Path;
use wt_config::{
    ActivityPane, BackendKind, Config, ConfigError, EffectTag, HarnessId, LoadOptions,
    ReviewBotMode, UiSort,
};

fn options(cwd: &Path, home: &Path, env: &[(&str, &str)]) -> LoadOptions {
    LoadOptions::new(
        cwd,
        home,
        env.iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
    )
}

fn toml_value(text: &str) -> toml::Value {
    toml::from_str(text).unwrap()
}

fn snapshot(config: &Config) -> serde_json::Value {
    use serde_json::{Value, json};
    let path = |value: &Path| value.to_string_lossy().into_owned();
    let number = |value: f64| {
        if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
            json!(value as i64)
        } else {
            json!(value)
        }
    };
    json!({
        "instance": { "role": match config.instance.role { wt_config::InstanceRole::Controller => "controller", wt_config::InstanceRole::Worker => "worker" } },
        "harness": { "primary": format!("{:?}", config.harness.primary).to_lowercase(), "hidden": config.harness.hidden.iter().map(|id| format!("{:?}", id).to_lowercase()).collect::<Vec<_>>() },
        "repoId": config.repo_id,
        "repoPath": path(&config.repo_path),
        "paths": {
            "mainClone": path(&config.paths.main_clone), "worktreeRoot": path(&config.paths.worktree_root),
            "logDir": path(&config.paths.log_dir), "appLogDir": path(&config.paths.app_log_dir),
            "lockDir": path(&config.paths.lock_dir), "cacheDb": path(&config.paths.cache_db),
            "stateDb": path(&config.paths.state_db), "cacheRoot": path(&config.paths.cache_root),
            "weztermCli": config.paths.wezterm_cli.as_deref().map(path), "dotfiles": path(&config.paths.dotfiles)
        },
        "tmux": { "socket": config.tmux.socket, "terminalConfig": config.tmux.terminal_config },
        "branch": { "prefix": config.branch.prefix, "base": config.branch.base, "idPattern": config.branch.id_pattern,
            "slugMaxLen": number(config.branch.slug_max_len), "keepFresh": config.branch.keep_fresh, "production": config.branch.production },
        "stage": { "prefix": config.stage.prefix, "defaultPersonal": config.stage.default_personal, "domain": config.stage.domain },
        "lifecycle": { "envFilesToCopy": config.lifecycle.env_files_to_copy, "copyGlobs": config.lifecycle.copy_globs,
            "installCommand": config.lifecycle.install_command, "destroyCommand": config.lifecycle.destroy_command },
        "backend": { "kind": if config.backend.kind == BackendKind::Rift { "rift" } else { "git-worktree" } },
        "sst": config.sst.as_ref().map(|sst| json!({ "stateBucket": sst.state_bucket, "statePrefix": sst.state_prefix, "awsProfile": sst.aws_profile, "autoRegenPaths": sst.auto_regen_paths })),
        "issueTracker": config.issue_tracker.as_ref().map(|tracker| json!({
            "urlTemplate": tracker.url_template, "prefix": tracker.prefix, "readCommand": tracker.read_command,
            "statusCommand": tracker.status_command, "statusStyles": tracker.status_styles.iter().map(|(name, style)|
                (name.clone(), json!({ "icon": format!("{:?}", style.icon).to_lowercase(), "color": style.color }))
            ).collect::<serde_json::Map<String, Value>>()
        })),
        "reviewBot": { "name": config.review_bot.name, "login": config.review_bot.login, "checkContexts": config.review_bot.check_contexts,
            "unresolvedVia": if config.review_bot.unresolved_via == ReviewBotMode::Checklist { "checklist" } else { "threads" },
            "summaryMarkers": config.review_bot.summary_marker, "pendingMarker": config.review_bot.pending_marker, "rerunCommand": config.review_bot.rerun_command },
        "devServer": config.dev_server.as_ref().map(|server| json!({ "command": server.command, "portBase": server.port_base,
            "portRange": server.port_range, "urlTemplate": server.url, "maxConcurrent": server.max_concurrent,
            "stopCommand": server.stop_command, "resetCommand": server.reset_command, "healthCommand": server.health_command })),
        "remote": config.remotes.first().map(|remote| json!({ "host": remote.host, "label": remote.label, "wtPath": remote.wt_path })),
        "diff": { "command": config.diff.command },
        "editor": { "command": config.editor.command },
        "browser": { "chromeProfile": config.browser.chrome_profile },
        "github": { "reviewers": config.github.reviewers, "ignoredReviewRepositories": config.github.ignored_review_repositories,
            "ignoredChecks": config.github.ignored_checks, "defaultReviewer": config.github.default_reviewer,
            "prTarget": if config.github.pr_target == wt_config::PullRequestTarget::Linear { "linear" } else { "github" },
            "events": config.github.events.as_ref().map(|event| json!({ "port": event.port, "host": event.host,
                "secret": event.secret, "secretFile": event.secret_file, "backstopPollMs": number(event.backstop_poll_ms) })) },
        "actions": config.actions.iter().map(|action| {
            let mut value = json!({ "kind": if action.kind == wt_config::ActionKind::Claude { "claude" } else { "shell" },
                "id": action.id, "name": action.name, "target": if action.target == wt_config::ActionTarget::Headless { "headless" } else if action.target == wt_config::ActionTarget::Session { "session" } else { "manager" },
                "affects": action.affects.as_ref().map(|tags| tags.iter().map(|tag| format!("{:?}", tag).to_lowercase()).collect::<Vec<_>>()),
                "requires": action.requires.iter().map(|tag| match tag { wt_config::RequireTag::Pr => "pr", wt_config::RequireTag::PrReady => "pr.ready", wt_config::RequireTag::Deployed => "deployed", wt_config::RequireTag::IssueTracker => "issue.tracker" }).collect::<Vec<_>>(),
                "argPrompt": action.arg_prompt.as_ref().map(|arg| json!({ "label": arg.label })), "labelExtract": action.label_extract });
            let object = value.as_object_mut().unwrap();
            if let Some(prompt) = &action.prompt { object.insert("prompt".into(), json!(prompt)); }
            if let Some(shell) = &action.shell { object.insert("shell".into(), json!(shell)); }
            if let Some(issue_status) = &action.issue_status { object.insert("issueStatus".into(), json!(issue_status)); }
            if let Some(key) = &action.key { object.insert("key".into(), json!(key)); }
            if let Some(group) = &action.group { object.insert("group".into(), json!(group)); }
            if action.external { object.insert("external".into(), json!(true)); }
            if action.kind == wt_config::ActionKind::Shell { object.remove("target"); }
            value
        }).collect::<Vec<_>>(),
        "automations": config.automations.iter().map(|automation| json!({ "id": automation.id,
            "on": match automation.on {
                wt_config::AutomationTrigger::PrChecksFailed => "pr.checks.failed",
                wt_config::AutomationTrigger::ReviewBotUnresolved => "review_bot.unresolved",
                wt_config::AutomationTrigger::ReviewChangesRequested => "review.changes_requested",
                wt_config::AutomationTrigger::PrConflict => "pr.conflict",
                wt_config::AutomationTrigger::PrMergeDropped => "pr.merge_dropped",
                wt_config::AutomationTrigger::WtMerged => "wt.merged",
                wt_config::AutomationTrigger::WtCreated => "wt.created",
                wt_config::AutomationTrigger::StackParentMerged => "stack.parent_merged",
                wt_config::AutomationTrigger::StatusNeedsHuman => "status.needs_human",
                wt_config::AutomationTrigger::StatusNeedsTesting => "status.needs_testing",
                wt_config::AutomationTrigger::StatusReady => "status.ready",
                wt_config::AutomationTrigger::StatusVerified => "status.verified",
                wt_config::AutomationTrigger::StatusVerificationDue => "status.verification_due",
                wt_config::AutomationTrigger::StatusVerificationOverdue => "status.verification_overdue",
                wt_config::AutomationTrigger::BranchAdvanced => "branch.advanced",
                wt_config::AutomationTrigger::StageDeployed => "stage.deployed",
                wt_config::AutomationTrigger::McpEvent => "mcp.event",
                wt_config::AutomationTrigger::WatchMet => "watch.met",
            },
            "run": automation.run, "busy": if automation.busy == wt_config::AutomationBusyPolicy::Skip { "skip" } else { "queue" },
            "cooldownMinutes": automation.cooldown_minutes.map(number), "afterDays": number(automation.after_days),
            "settleSeconds": number(automation.settle_seconds), "branch": automation.branch } )).collect::<Vec<_>>(),
        "ui": { "rows": config.ui.rows, "hiddenBadges": config.ui.hidden_badges.iter().cloned().collect::<Vec<_>>(),
            "sort": if config.ui.sort == wt_config::UiSort::Manual { "manual" } else { "status" },
            "activityPane": if config.ui.activity_pane == wt_config::ActivityPane::FullWidth { "full_width" } else { "column" },
            "hideTerminalApps": config.ui.hide_terminal_apps, "actionGroupsLast": config.ui.action_groups_last },
        "skills": { "startupCheck": config.skills.startup_check },
        "manager": { "wtFeedback": config.manager.wt_feedback },
        "update": { "startupCheck": config.update.startup_check }
    })
}

#[test]
fn all_options_match_the_typescript_loader_golden_snapshot() {
    let root = Path::new("/tmp/wt-config-fixture");
    let fixture = include_str!("fixtures/all-options.toml");
    let home = root.join("home");
    let options = options(&root.join("run"), &home, &[]);
    let config = Config::from_raw(toml_value(fixture), &options).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/typescript-all-options.json")).unwrap();
    assert_eq!(snapshot(&config), expected);
}

fn minimal_config(root: &Path) -> String {
    format!(
        "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n\n[branch]\nprefix = 'm'\n",
        root.join("main").display().to_string(),
        root.join("worktrees").display().to_string()
    )
}

#[test]
fn only_supported_session_harnesses_are_accepted_and_one_must_remain_visible() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path(), root.path(), &[]);
    let base = minimal_config(root.path());
    let pi =
        format!("{base}\n[harness]\nprimary = 'pi'\nhidden = ['claude', 'codex', 'opencode']\n");
    assert_eq!(
        Config::from_raw(toml_value(&pi), &opts)
            .unwrap()
            .harness
            .primary,
        HarnessId::Pi
    );
    let unsupported = format!("{base}\n[harness]\nprimary = 'unknown-agent'\n");
    assert!(Config::from_raw(toml_value(&unsupported), &opts).is_err());
    let all_hidden = format!("{base}\n[harness]\nhidden = ['claude', 'codex', 'opencode', 'pi']\n");
    assert!(Config::from_raw(toml_value(&all_hidden), &opts).is_err());
}

#[test]
fn wt_session_harness_is_opt_in_validated_and_must_stay_visible() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path(), root.path(), &[]);
    let base = minimal_config(root.path());
    let unset = Config::from_raw(toml_value(&base), &opts).unwrap();
    assert_eq!(unset.harness.wt_session, None);

    let pinned =
        format!("{base}\n[harness]\nprimary = 'codex'\nhidden = ['pi']\nwt_session = 'claude'\n");
    let config = Config::from_raw(toml_value(&pinned), &opts).unwrap();
    assert_eq!(config.harness.primary, HarnessId::Codex);
    assert_eq!(config.harness.wt_session, Some(HarnessId::Claude));

    let empty = format!("{base}\n[harness]\nwt_session = ''\n");
    assert_eq!(
        Config::from_raw(toml_value(&empty), &opts)
            .unwrap()
            .harness
            .wt_session,
        None
    );

    let unknown = format!("{base}\n[harness]\nwt_session = 'gpt'\n");
    let error = Config::from_raw(toml_value(&unknown), &opts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("harness.wt_session"), "{error}");

    let hidden = format!("{base}\n[harness]\nhidden = ['codex']\nwt_session = 'codex'\n");
    let error = Config::from_raw(toml_value(&hidden), &opts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("harness.wt_session"), "{error}");
}

#[test]
fn wt_session_claude_token_command_is_an_optional_argv() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path(), root.path(), &[]);
    let base = minimal_config(root.path());
    let unset = Config::from_raw(toml_value(&base), &opts).unwrap();
    assert_eq!(unset.harness.wt_session_claude_token_command, None);

    let set = format!(
        "{base}\n[harness]\nwt_session = 'claude'\nwt_session_claude_token_command = ['/usr/bin/security', 'find-generic-password', '-s', 'wt-claude', '-w']\n"
    );
    let config = Config::from_raw(toml_value(&set), &opts).unwrap();
    assert_eq!(
        config.harness.wt_session_claude_token_command.as_deref(),
        Some(
            &[
                "/usr/bin/security".to_owned(),
                "find-generic-password".into(),
                "-s".into(),
                "wt-claude".into(),
                "-w".into()
            ][..]
        )
    );
    // The account selector is independent of the harness pin.
    let unpinned = format!("{base}\n[harness]\nwt_session_claude_token_command = ['helper']\n");
    let config = Config::from_raw(toml_value(&unpinned), &opts).unwrap();
    assert_eq!(config.harness.wt_session, None);
    assert!(config.harness.wt_session_claude_token_command.is_some());

    for invalid in ["[]", "['']", "['  ', 'x']"] {
        let raw = format!("{base}\n[harness]\nwt_session_claude_token_command = {invalid}\n");
        let error = Config::from_raw(toml_value(&raw), &opts)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("harness.wt_session_claude_token_command"),
            "{invalid}: {error}"
        );
    }
}

#[test]
fn wt_session_claude_args_are_appended_flags_that_leave_identity_to_wt() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path(), root.path(), &[]);
    let base = minimal_config(root.path());
    let unset = Config::from_raw(toml_value(&base), &opts).unwrap();
    assert!(unset.harness.wt_session_claude_args.is_empty());

    let set = format!(
        "{base}\n[harness]\nwt_session_claude_args = ['--permission-mode', 'acceptEdits', '--model=opus']\n"
    );
    let config = Config::from_raw(toml_value(&set), &opts).unwrap();
    assert_eq!(
        config.harness.wt_session_claude_args,
        ["--permission-mode", "acceptEdits", "--model=opus"]
    );

    for invalid in [
        "['--resume', 'x']",
        "['--session-id=x']",
        "['-p']",
        "['--bare']",
    ] {
        let raw = format!("{base}\n[harness]\nwt_session_claude_args = {invalid}\n");
        let error = Config::from_raw(toml_value(&raw), &opts)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("harness.wt_session_claude_args cannot include"),
            "{invalid}: {error}"
        );
    }
}

#[test]
fn claude_env_sets_features_but_never_the_account() {
    let root = tempfile::tempdir().unwrap();
    let opts = options(root.path(), root.path(), &[]);
    let base = minimal_config(root.path());
    let unset = Config::from_raw(toml_value(&base), &opts).unwrap();
    assert!(unset.harness.claude_env.is_empty());

    let set = format!("{base}\n[harness.claude_env]\nCLAUDE_CODE_EXPERIMENTAL_CODE_MODE = '1'\n");
    let config = Config::from_raw(toml_value(&set), &opts).unwrap();
    assert_eq!(
        config
            .harness
            .claude_env
            .get("CLAUDE_CODE_EXPERIMENTAL_CODE_MODE"),
        Some(&"1".to_owned())
    );

    for invalid in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CONFIG_DIR",
        "PATH",
        "WT_AGENT",
        "lower_case",
    ] {
        let raw = format!("{base}\n[harness.claude_env]\n{invalid} = 'x'\n");
        let error = Config::from_raw(toml_value(&raw), &opts)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("harness.claude_env.{invalid}")),
            "{invalid}: {error}"
        );
    }
}

#[test]
fn defaults_are_typed_and_repository_derived() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let main = root.path().join("main");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&main).unwrap();
    let options = options(root.path(), &home, &[]);
    let config = Config::from_raw(toml_value(&minimal_config(root.path())), &options).unwrap();

    assert_eq!(config.branch.prefix, "m");
    assert_eq!(config.branch.base, "main");
    assert_eq!(config.stage.prefix, "m-");
    assert_eq!(config.stage.default_personal, "m");
    assert_eq!(config.harness.primary, HarnessId::Claude);
    assert!(config.paths.wt_source.is_none());
    assert!(config.github.reviewers);
    assert_eq!(
        config.github.pr_target,
        wt_config::PullRequestTarget::Github
    );
    assert_eq!(config.ui.sort, UiSort::Status);
    assert_eq!(config.ui.activity_pane, ActivityPane::Column);
    assert!(!config.ui.owed_sections);
    assert_eq!(
        config.ui.rows,
        ["branch", "issue", "stage", "dev", "pr", "claude", "git"]
    );
    assert_eq!(config.lifecycle.env_files_to_copy, [".env"]);
    assert_eq!(
        config.diff.command,
        "revdiff --vim-motion --compact {{base}}"
    );
    assert_eq!(config.actions.len(), 2);
    assert_eq!(
        config.actions[0].affects,
        Some(vec![EffectTag::Git, EffectTag::Github])
    );
    assert!(config.repo_id.contains("main"));
    assert_eq!(
        config.paths.cache_root,
        config.paths.cache_db.parent().unwrap()
    );
    assert_eq!(
        config.paths.state_db,
        config.paths.cache_root.join("wt.sqlite")
    );
}

#[test]
fn owed_sections_are_opt_in_and_must_be_a_boolean() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let load = |ui: &str| {
        let text = format!("{}\n[ui]\n{ui}\n", minimal_config(root.path()));
        Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
    };
    assert!(!load("").unwrap().ui.owed_sections);
    assert!(!load("owed_sections = false").unwrap().ui.owed_sections);
    let on = load("owed_sections = true\nsort = 'manual'").unwrap();
    assert!(on.ui.owed_sections);
    // The option leaves every other ui default alone.
    assert_eq!(on.ui.sort, UiSort::Manual);
    assert_eq!(on.ui.rows, load("").unwrap().ui.rows);
    let error = load("owed_sections = 'yes'").unwrap_err().to_string();
    assert!(error.contains("ui"), "{error}");
    let fixture = toml_value(include_str!("fixtures/all-options.toml"));
    assert_eq!(fixture["ui"]["owed_sections"].as_bool(), Some(true));
}

#[test]
fn strings_and_arrays_home_expand_and_defaults_replace_explicit_empty() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let text = format!(
        "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\ncache_db = '~/cache/cache.sqlite'\nwt_source = '~/Code/wt'\n\n[branch]\nprefix = 'm'\nkeep_fresh = ['release']\n\n[issue_tracker]\nurl_template = 'https://tasks/{{id}}'\nstatus_command = ['~/bin/status', '{{ids}}']\n\n[review_bot]\nunresolved_via = 'checklist'\nlogin = 'reviewer[bot]'\nsummary_marker = ['one', 'two']\n",
        root.path().join("main").display().to_string(),
        root.path().join("worktrees").display().to_string()
    );
    let opts = options(root.path(), &home, &[]);
    let config = Config::from_raw(toml_value(&text), &opts).unwrap();

    assert_eq!(config.paths.cache_db, home.join("cache/cache.sqlite"));
    assert_eq!(config.paths.wt_source, Some(home.join("Code/wt")));
    assert_eq!(
        config
            .issue_tracker
            .as_ref()
            .unwrap()
            .status_command
            .as_ref()
            .unwrap()[0],
        home.join("bin/status").display().to_string()
    );
    assert_eq!(config.review_bot.unresolved_via, ReviewBotMode::Checklist);
    assert_eq!(config.review_bot.login, "reviewer");
    assert_eq!(config.review_bot.summary_marker, ["one", "two"]);
}

#[test]
fn user_and_repository_configs_merge_tables_and_replace_arrays() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config_dir = home.join(".config/wt");
    let main = root.path().join("main");
    let worktrees = root.path().join("worktrees");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&main).unwrap();
    fs::create_dir_all(&worktrees).unwrap();
    fs::write(config_dir.join("config.toml"), "[branch]\nprefix = 'personal'\nbase = 'trunk'\nkeep_fresh = ['release', 'main']\n[ui]\nrows = ['pr', 'git']\n").unwrap();
    fs::write(main.join(".wt.toml"), format!("[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n[branch]\nprefix = 'repo'\nkeep_fresh = ['release']\n[ui]\nrows = ['issue']\n", main.display().to_string(), worktrees.display().to_string())).unwrap();
    let opts = options(&main, &home, &[]);
    let config = Config::load(&opts).unwrap();

    assert_eq!(config.branch.prefix, "repo");
    assert_eq!(config.branch.base, "trunk");
    assert_eq!(config.branch.keep_fresh, ["release"]);
    assert_eq!(config.ui.rows, ["issue"]);
    assert_eq!(config.repository_config, Some(main.join(".wt.toml")));
    assert_eq!(config.tmux.socket, format!("wt-{}", config.repo_id));
}

#[test]
fn linked_worktree_uses_main_clone_config_without_inherited_repo_env() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let main = root.path().join("main");
    let wt_root = root.path().join("worktrees");
    let worktree = wt_root.join("feature");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(main.join(".git")).unwrap();
    fs::create_dir_all(worktree.join("subdir")).unwrap();
    let gitdir = main.join(".git/worktrees/feature");
    fs::create_dir_all(&gitdir).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", gitdir.display()),
    )
    .unwrap();
    fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    fs::write(
        main.join(".wt.toml"),
        format!(
            "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n[branch]\nprefix = 'repo'\n",
            main.display().to_string(),
            wt_root.display().to_string()
        ),
    )
    .unwrap();
    // The ignored repository config supplies the required paths, so a fresh
    // environment needs no global config and no inherited WT_REPO_CONFIG.
    let opts = options(&worktree.join("subdir"), &home, &[]);
    let config = Config::load(&opts).unwrap();

    assert_eq!(config.repository_config, Some(main.join(".wt.toml")));
    assert_eq!(config.branch.prefix, "repo");
    assert!(config.repo_id.ends_with("-main"));
}

#[test]
fn missing_required_values_are_reported_together_and_user_file_is_required() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let opts = options(root.path(), &home, &[]);
    let error = Config::from_raw(toml_value(""), &opts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("paths.main_clone is required"));
    assert!(error.contains("paths.worktree_root is required"));
    assert!(error.contains("branch.prefix is required"));

    let error = Config::load(&opts).unwrap_err();
    assert!(matches!(error, ConfigError::NotFound { .. }));
}

#[test]
fn invalid_automation_pairs_and_issue_status_contract_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut text = minimal_config(root.path());
    text.push_str("\n[[actions]]\nid='status'\nname='Set status'\nshell='true'\nissue_status='Doing'\n\n[[automations]]\nid='bad'\non='pr.checks.failed'\nrun='builtin:restack'\n");
    let error = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("actions[0].issue_status requires affects = [\"issue\"]"));
    assert!(error.contains("run \"builtin:restack\" requires on = \"stack.parent_merged\""));
}

#[test]
fn an_unknown_trigger_skips_its_rule_with_a_warning_instead_of_rejecting_the_config() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut text = minimal_config(root.path());
    text.push_str(
        "\n[[automations]]\nid='future'\non='status.from_a_newer_wt'\nrun='builtin:notify'\nbusy='a-newer-policy'\nnew_field=1\n\
         [[automations]]\nid='kept'\non='status.ready'\nrun='builtin:notify'\n",
    );
    let config = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[])).unwrap();
    assert_eq!(
        config
            .automations
            .iter()
            .map(|rule| rule.id.as_str())
            .collect::<Vec<_>>(),
        ["kept"]
    );
    assert_eq!(config.warnings.len(), 1);
    assert!(config.warnings[0].contains("\"future\""));
    assert!(config.warnings[0].contains("status.from_a_newer_wt"));

    let mut missing = minimal_config(root.path());
    missing.push_str("\n[[automations]]\nid='blank'\nrun='builtin:notify'\n");
    let error = Config::from_raw(toml_value(&missing), &options(root.path(), &home, &[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("automations[0].on is required"), "{error}");
}

/// A key the palettes keep for themselves used to be swapped for another
/// letter without a word. The config still loads, and says so.
#[test]
fn an_action_key_a_palette_keeps_is_said_and_never_silently_replaced() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let action = |id: &str, key: &str, body: &str| {
        format!("\n[[actions]]\nid='{id}'\nname='{id}'\nkey='{key}'\n{body}\n")
    };
    let shell = "shell='true'";
    let load =
        |text: &str| Config::from_raw(toml_value(text), &options(root.path(), &home, &[])).unwrap();
    let mut text = minimal_config(root.path());
    for (id, key) in [("quit", "q"), ("merge", "m"), ("free", "x"), ("again", "x")] {
        text.push_str(&action(id, key, shell));
    }
    // d and s are the palette's only where a dev server is configured.
    text.push_str(&action("deploy", "d", shell));
    // A slot's action is listed on no palette that takes configured keys.
    text.push_str(&action("slot", "q", "prompt='go'\ntarget='slot'"));
    let config = load(&text);
    assert_eq!(config.actions.len(), 6, "a warning never drops an action");
    let warned = |id: &str| {
        config
            .warnings
            .iter()
            .find(|warning| warning.contains(&format!("(\"{id}\")")))
    };
    let quit = warned("quit").expect("q closes the palette");
    assert!(quit.contains("key = \"q\" is not used"), "{quit}");
    assert!(quit.contains("closing the palette"), "{quit}");
    assert!(warned("merge").unwrap().contains("merge when ready"));
    assert!(warned("again").unwrap().contains("\"free\" already has it"));
    for id in ["free", "deploy", "slot"] {
        assert!(warned(id).is_none(), "{id}: {:?}", config.warnings);
    }
    assert_eq!(config.warnings.len(), 3);

    text.push_str("\n[dev_server]\ncommand='true'\n");
    let config = load(&text);
    assert!(
        config
            .warnings
            .iter()
            .any(|warning| warning.contains("(\"deploy\")")
                && warning.contains("starting the dev server")),
        "{:?}",
        config.warnings
    );
}

#[test]
fn action_and_automation_aliases_normalize_with_defaults_and_constraints() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut text = minimal_config(root.path());
    text.push_str(
        "\nbase='main'\nkeep_fresh=['release']\n\
         [[actions]]\nid='triage'\nname='Triage issue'\nshell='echo {{issue}}'\n\
         affects=['issue','issue']\nrequires=['issue.tracker','pr.ready']\n\
         issue_status='Doing'\nkey='t'\ngroup='Issues'\nexternal=true\nretry_on_failure=true\n\
         arg_prompt='Issue title'\nlabel_extract='^([A-Z]+-\\d+)'\n\
         [[automations]]\nid='legacy-review'\non='rabbit.unresolved'\nrun='triage'\n\
         [[automations]]\nid='needs-human'\non='status.needs-human'\nrun='builtin:notify'\n\
         [[automations]]\nid='advance-release'\non='branch.advanced'\nrun='builtin:notify'\nbranch=' release '\n\
         [[automations]]\nid='overdue'\non='status.verification_overdue'\nrun='builtin:notify'\nafter_days=4\n\
         [[automations]]\nid='dropped'\non='pr.merge_dropped'\nrun='builtin:notify'\n",
    );
    let config = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[])).unwrap();

    assert_eq!(config.actions.len(), 1);
    let action = &config.actions[0];
    assert_eq!(action.kind, wt_config::ActionKind::Shell);
    assert_eq!(action.affects, Some(vec![EffectTag::Issue]));
    assert_eq!(
        action.requires,
        [
            wt_config::RequireTag::IssueTracker,
            wt_config::RequireTag::PrReady
        ]
    );
    assert_eq!(action.arg_prompt.as_ref().unwrap().label, "Issue title");
    assert_eq!(action.label_extract.as_deref(), Some("^([A-Z]+-\\d+)"));
    assert!(action.external);
    assert!(action.retry_on_failure);
    let prompt = format!(
        "{}\n[[actions]]\nid='p'\nname='P'\nprompt='go'\nretry_on_failure=true\n",
        minimal_config(root.path())
    );
    let error = Config::from_raw(toml_value(&prompt), &options(root.path(), &home, &[]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("retry_on_failure is only valid on a shell action"),
        "{error}"
    );

    assert_eq!(config.automations.len(), 5);
    assert_eq!(
        config.automations[0].on,
        wt_config::AutomationTrigger::ReviewBotUnresolved
    );
    assert_eq!(config.automations[0].settle_seconds, 120.0);
    assert_eq!(
        config.automations[1].on,
        wt_config::AutomationTrigger::StatusNeedsHuman
    );
    assert_eq!(config.automations[1].settle_seconds, 0.0);
    assert_eq!(config.automations[2].branch.as_deref(), Some("release"));
    assert_eq!(config.automations[2].settle_seconds, 10.0);
    assert_eq!(config.automations[3].after_days, 4.0);
    assert_eq!(config.automations[3].settle_seconds, 0.0);
    assert_eq!(
        config.automations[4].on,
        wt_config::AutomationTrigger::PrMergeDropped
    );
    // GitHub's reads lag an arming; the default settle rides that out.
    assert_eq!(config.automations[4].settle_seconds, 120.0);
}

#[test]
fn action_target_slot_remains_config_compatible() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let text = format!(
        "{}\n[[actions]]\nid='slot-command'\nname='Slot command'\nprompt='Continue current work'\ntarget='slot'\n",
        minimal_config(root.path())
    );
    let config = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[])).unwrap();
    assert_eq!(config.actions[0].target, wt_config::ActionTarget::Slot);
}

#[test]
fn environment_wins_for_config_path_and_tmux_socket() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let chosen = root.path().join("chosen.toml");
    fs::write(&chosen, minimal_config(root.path())).unwrap();
    let opts = options(
        root.path(),
        &home,
        &[
            ("WT_CONFIG", chosen.to_str().unwrap()),
            ("WT_TMUX_SOCKET", "custom-socket"),
        ],
    );
    let config = Config::load(&opts).unwrap();
    assert_eq!(config.tmux.socket, "custom-socket");
}

#[test]
fn optional_sections_keep_their_typed_values_and_defaults() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let text = format!(
        "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n\n[branch]\nprefix='m'\n\n[backend]\nkind = 'rift'\n\n[deploy.sst]\nstate_bucket = 'state'\nstate_prefix = 'apps/'\naws_profile = 'dev'\n\n[issue_tracker]\nprefix = 'acme'\n[issue_tracker.linear]\nworkspace = 'acme'\n\n[dev_server]\ncommand = 'pnpm dev'\nmax_concurrent = 3\nstop_command = 'stop'\nreset_command = 'reset'\nhealth_command = 'check'\n\n[remote]\nhost = 'worker'\n\n[naming]\nharness = 'codex'\nreasoning_effort = 'xhigh'\nmodels = {{ codex = 'gpt' }}\n\n[github]\nreviewers = false\npr_target = 'linear'\nignored_checks = ['bot']\n[github.events]\nsecret_file = '~/secret'\n\n[editor]\ncommand = 'code {{{{path}}}}'\n[browser]\nchrome_profile = 'legacy'\n[tmux]\nsocket = 'override'\nterminal_config = ''\n[skills]\nstartup_check = false\n[manager]\nwt_feedback = true\n[update]\nstartup_check = false\n",
        root.path().join("main").display().to_string(),
        root.path().join("worktrees").display().to_string()
    );
    let config = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[])).unwrap();

    assert_eq!(config.backend.kind, BackendKind::Rift);
    assert_eq!(
        config.sst.as_ref().unwrap().auto_regen_paths,
        ["sst-env.d.ts"]
    );
    assert_eq!(
        config
            .issue_tracker
            .as_ref()
            .unwrap()
            .url_template
            .as_deref(),
        Some("linear://acme/issue/{id}")
    );
    assert_eq!(
        config.issue_tracker.as_ref().unwrap().prefix.as_deref(),
        Some("acme")
    );
    assert_eq!(config.dev_server.as_ref().unwrap().port_base, 8100);
    assert_eq!(
        config.dev_server.as_ref().unwrap().url,
        "http://localhost:{{port}}/"
    );
    assert_eq!(config.remotes[0].label, "worker");
    assert_eq!(config.remotes[0].wt_path, "~/.wt/bin/wt");
    assert_eq!(
        config
            .github
            .events
            .as_ref()
            .unwrap()
            .secret_file
            .as_deref(),
        Some(home.join("secret").to_string_lossy().as_ref())
    );
    assert_eq!(config.editor.command.as_deref(), Some("code {{path}}"));
    assert_eq!(config.browser.chrome_profile.as_deref(), Some("legacy"));
    assert_eq!(config.tmux.socket, "override");
    assert_eq!(config.tmux.terminal_config.as_deref(), Some(""));
    assert!(!config.skills.startup_check);
    assert!(config.manager.wt_feedback);
    assert!(!config.update.startup_check);
}

#[test]
fn explicit_repository_config_environment_overrides_cwd_discovery() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(home.join("config.toml"), "[branch]\nprefix='user'\n").unwrap();
    fs::write(
        first.join(".wt.toml"),
        format!(
            "[paths]\nmain_clone={:?}\nworktree_root={:?}\n[branch]\nprefix='first'\n",
            first.display().to_string(),
            root.path().join("wt1").display().to_string()
        ),
    )
    .unwrap();
    fs::write(
        second.join(".wt.toml"),
        format!(
            "[paths]\nmain_clone={:?}\nworktree_root={:?}\n[branch]\nprefix='second'\n",
            second.display().to_string(),
            root.path().join("wt2").display().to_string()
        ),
    )
    .unwrap();
    let opts = options(
        &first,
        &home,
        &[
            ("WT_CONFIG", home.join("config.toml").to_str().unwrap()),
            ("WT_REPO_CONFIG", second.join(".wt.toml").to_str().unwrap()),
        ],
    );
    let config = Config::load(&opts).unwrap();

    assert_eq!(config.repository_config, Some(second.join(".wt.toml")));
    assert_eq!(config.branch.prefix, "second");
}

#[test]
fn startup_checks_default_on_when_their_sections_are_absent() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let opts = options(root.path(), &home, &[]);
    let config = Config::from_raw(toml_value(&minimal_config(root.path())), &opts).unwrap();
    assert!(config.skills.startup_check);
    assert!(config.update.startup_check);
    let text = format!("{}\n[skills]\n[update]\n", minimal_config(root.path()));
    let config = Config::from_raw(toml_value(&text), &opts).unwrap();
    assert!(config.skills.startup_check);
    assert!(config.update.startup_check);
}

#[test]
fn an_overflowing_dev_server_port_range_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let text = format!(
        "{}\n[dev_server]\ncommand = 'serve'\nport_base = 8100\nport_range = 4294967295\n",
        minimal_config(root.path())
    );
    let error = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("dev_server.port_range"), "{error}");
}

#[test]
fn from_raw_selects_the_same_repository_identity_as_load_in_a_worktree_copy() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let main = root.path().join("main");
    let wt_root = root.path().join("worktrees");
    let worktree = wt_root.join("feature");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&main).unwrap();
    fs::create_dir_all(&worktree).unwrap();
    let text = format!(
        "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\n[branch]\nprefix = 'repo'\n",
        main.display().to_string(),
        wt_root.display().to_string()
    );
    fs::write(main.join(".wt.toml"), &text).unwrap();
    // A copy carried into the worktree (committed or copied by setup).
    fs::write(worktree.join(".wt.toml"), &text).unwrap();
    let opts = options(&worktree, &home, &[]);
    let loaded = Config::load(&opts).unwrap();
    let raw = Config::from_raw(toml_value(&text), &opts).unwrap();

    assert_eq!(
        loaded.repository_config,
        Some(fs::canonicalize(&main).unwrap().join(".wt.toml"))
    );
    assert_eq!(raw.repository_config, loaded.repository_config);
    assert_eq!(raw.repo_id, loaded.repo_id);
    assert_eq!(raw.paths.state_db, loaded.paths.state_db);
    assert_eq!(raw.tmux.socket, loaded.tmux.socket);
}

#[test]
fn mcp_event_rules_name_a_server_an_event_and_a_prompt_action() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut text = minimal_config(root.path());
    text.push_str(
        "\n[mcp_servers.boo]\nurl='https://cozee.artifacts.boo/mcp'\ntoken_command=['security','find-generic-password','-w']\n\
         [[actions]]\nid='answer'\nname='Answer'\nprompt='Answer the comment'\ntarget='session'\n\
         [[automations]]\nid='comments'\non='mcp.event'\nrun='answer'\nserver='boo'\nevent='comment.created'\n\
         [automations.arguments]\nartifact='a9ib'\n",
    );
    let config = Config::from_raw(toml_value(&text), &options(root.path(), &home, &[])).unwrap();
    let rule = &config.automations[0];
    assert_eq!(rule.on, wt_config::AutomationTrigger::McpEvent);
    assert_eq!(rule.event.as_deref(), Some("comment.created"));
    assert_eq!(rule.arguments["artifact"], serde_json::json!("a9ib"));
    assert_eq!(rule.settle_seconds, 0.0);
    assert_eq!(
        config.mcp_servers["boo"].url,
        "https://cozee.artifacts.boo/mcp"
    );

    let mut bad = minimal_config(root.path());
    bad.push_str(
        "\n[mcp_servers.plain]\nurl='https://example.com/mcp'\n[mcp_servers.insecure]\nurl='http://example.com/mcp'\n\
         [[actions]]\nid='sh'\nname='Shell'\nshell='echo {{event}}'\n\
         [[automations]]\nid='a'\non='mcp.event'\nrun='sh'\nserver='plain'\nevent='x'\n\
         [[automations]]\nid='b'\non='mcp.event'\nrun='builtin:notify'\nserver='missing'\nevent='x'\n\
         [[automations]]\nid='c'\non='pr.conflict'\nrun='builtin:notify'\nevent='x'\n\
         [mcp_servers.sneaky]\nurl='http://localhost:80@evil.example/mcp'\n\
         [[actions]]\nid='auto'\nname='Auto'\nprompt='Go'\n\
         [[automations]]\nid='d'\non='mcp.event'\nrun='auto'\nserver='plain'\nevent='x'\n",
    );
    let error = Config::from_raw(toml_value(&bad), &options(root.path(), &home, &[]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("mcp_servers.insecure.url must be an https:// URL"),
        "{error}"
    );
    assert!(
        error.contains("never a shell, headless or slot action"),
        "{error}"
    );
    assert!(
        error.contains("automations[1].server \"missing\" is not a [mcp_servers] entry"),
        "{error}"
    );
    assert!(
        error.contains("only apply to on = \"mcp.event\""),
        "{error}"
    );
    assert!(
        error.contains("mcp_servers.sneaky.url must name a host and no credentials"),
        "{error}"
    );
    assert!(error.contains("automations[3]"), "{error}");
}

#[test]
fn checks_and_notify_are_argv_and_the_watch_trigger_is_reserved() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let load = |extra: &str| {
        let mut text = minimal_config(root.path());
        text.push_str(extra);
        Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
    };
    let config = load("").unwrap();
    assert!(config.checks.is_empty());
    assert_eq!(config.notify.command, None);

    let config = load(
        "\n[[checks]]\nid = 'secret'\ndescription = 'a secret is set'\nrun = ['vault', 'has', '{{arg}}']\n\
         [[checks]]\nid = 'flag.on'\nrun = ['flag']\nevery_seconds = 30\ntimeout_seconds = 5\n\
         [notify]\ncommand = ['push', '{{title}}', '{{body}}']\n",
    )
    .unwrap();
    assert_eq!(config.checks[0].id, "secret");
    assert_eq!(config.checks[0].description, "a secret is set");
    assert_eq!(config.checks[0].run, ["vault", "has", "{{arg}}"]);
    assert_eq!(
        (
            config.checks[0].every_seconds,
            config.checks[0].timeout_seconds
        ),
        (300, 30)
    );
    assert_eq!(
        (
            config.checks[1].every_seconds,
            config.checks[1].timeout_seconds
        ),
        (30, 5)
    );
    assert_eq!(
        config.notify.command.as_deref().unwrap(),
        ["push", "{{title}}", "{{body}}"]
    );

    for (extra, expected) in [
        (
            "\n[checks]\nid = 'x'\n",
            "checks must be an array of [[checks]] tables",
        ),
        ("\n[[checks]]\nrun = ['x']\n", "checks[0].id is required"),
        (
            "\n[[checks]]\nid = 'a=b'\nrun = ['x']\n",
            "checks[0].id is required and may hold only",
        ),
        (
            "\n[[checks]]\nid = 'a'\nrun = ['x']\n[[checks]]\nid = 'a'\nrun = ['y']\n",
            "checks[1].id \"a\" is duplicated",
        ),
        (
            "\n[[checks]]\nid = 'a'\nrun = []\n",
            "checks[0].run must be an array of nonempty strings",
        ),
        (
            "\n[[checks]]\nid = 'a'\nrun = 'vault has x'\n",
            "checks[0]: ",
        ),
        // The watch's argument may fill an argument, never pick the program.
        (
            "\n[[checks]]\nid = 'a'\nrun = ['{{arg}}', 'x']\n",
            "checks[0].run[0] is the program to run and cannot be a template",
        ),
        (
            "\n[[checks]]\nid = 'a'\nrun = ['bin/{{slug}}']\n",
            "checks[0].run[0] is the program to run and cannot be a template",
        ),
        (
            "\n[[checks]]\nid = 'a'\nrun = ['x']\nevery_seconds = 29\n",
            "checks[0].every_seconds must be at least 30",
        ),
        (
            "\n[[checks]]\nid = 'a'\nrun = ['x']\ntimeout_seconds = 0\n",
            "checks[0].timeout_seconds must be a positive integer",
        ),
        (
            "\n[notify]\ncommand = []\n",
            "notify.command must be an array of nonempty strings",
        ),
        ("\n[notify]\ncommand = 'push it'\n", "notify: "),
        (
            "\n[[automations]]\nid = 'w'\non = 'watch.met'\nrun = 'builtin:notify'\n",
            "automations[0].on \"watch.met\" is wt's own trigger for watches",
        ),
        (
            "\n[[automations]]\nid = 'w'\non = 'status.ready'\nrun = 'builtin:watch'\n",
            "automations[0].run \"builtin:watch\" is neither",
        ),
        (
            "\n[[automations]]\nid = 'w'\non = 'status.ready'\nrun = 'builtin:check'\n",
            "automations[0].run \"builtin:check\" is neither",
        ),
        // Fire keys and history under these ids belong to wt's own rules.
        (
            "\n[[automations]]\nid = 'wt.watch'\non = 'status.ready'\nrun = 'builtin:notify'\n",
            "automations[0].id \"wt.watch\" is reserved for a rule wt runs itself",
        ),
        (
            "\n[[automations]]\nid = 'wt.check'\non = 'status.verification_due'\nrun = 'builtin:notify'\n",
            "automations[0].id \"wt.check\" is reserved for a rule wt runs itself",
        ),
    ] {
        let error = load(extra).unwrap_err().to_string();
        assert!(error.contains(expected), "{extra}: {error}");
    }
}

#[test]
fn history_lists_the_skills_it_records_by_name() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let load = |extra: &str| {
        let mut text = minimal_config(root.path());
        text.push_str(extra);
        Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
    };
    let config = load("").unwrap();
    assert!(config.history.skills.is_empty());
    assert!(!config.history.records_skill("check"));

    let config = load("\n[history]\nskills = ['/fable-review', 'check', 'tools:lint']\n").unwrap();
    assert_eq!(
        config.history.skills,
        ["fable-review", "check", "tools:lint"]
    );
    for (name, recorded) in [
        ("fable-review", true),
        ("/check", true),
        ("Check", false),
        ("checks", false),
        // A plugin's skill is listed whole or by its own name.
        ("other:check", true),
        ("tools:lint", true),
        ("lint", false),
        ("check:other", false),
    ] {
        assert_eq!(config.history.records_skill(name), recorded, "{name}");
    }
    let every = load("\n[history]\nskills = ['*']\n").unwrap();
    assert!(every.history.records_skill("anything:at-all"));

    let stamping = load("\n[history]\nstamps = ['/fable-review']\n").unwrap();
    assert_eq!(stamping.history.stamps, ["fable-review"]);
    // A stamping skill is recorded without being listed in skills.
    assert!(stamping.history.records_skill("fable-review"));
    assert_eq!(
        stamping.history.stamp_for("tools:fable-review"),
        Some("fable-review")
    );
    assert_eq!(stamping.history.stamp_for("check"), None);
    assert!(!stamping.history.is_empty());

    for (extra, expected) in [
        (
            "\n[history]\nstamps = ['*']\n",
            "history.stamps must be an array of skill or slash-command names",
        ),
        ("\n[history]\nskills = 'check'\n", "history: "),
        (
            "\n[history]\nskills = ['check', '']\n",
            "history.skills must be an array of skill or slash-command names",
        ),
        (
            "\n[history]\nskills = ['/']\n",
            "history.skills must be an array of skill or slash-command names",
        ),
        (
            "\n[history]\nskills = ['fable review']\n",
            "history.skills must be an array of skill or slash-command names",
        ),
    ] {
        let error = load(extra).unwrap_err().to_string();
        assert!(error.contains(expected), "{extra}: {error}");
    }
}

#[test]
fn environments_name_a_deploy_probe_per_stage_and_reject_anything_else() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let load = |extra: &str| {
        let mut text = minimal_config(root.path());
        text.push_str(extra);
        Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
    };
    let config = load("").unwrap();
    assert!(config.environments.is_empty());

    let config = load(
        "\n[environments.staging]\ndeployed = ['deploy-tool', 'sha', 'staging']\n\
         [environments.production]\ndeployed = ['deploy-tool', 'sha']\nevery_seconds = 15\ntimeout_seconds = 5\n",
    )
    .unwrap();
    let staging = config
        .environments
        .get(wt_core::VerifyStage::Staging)
        .unwrap();
    assert_eq!(staging.deployed, ["deploy-tool", "sha", "staging"]);
    assert_eq!((staging.every_seconds, staging.timeout_seconds), (60, 20));
    let production = config.environments.production.as_ref().unwrap();
    assert_eq!(
        (production.every_seconds, production.timeout_seconds),
        (15, 5)
    );
    assert_eq!(config.environments.probed().count(), 2);

    for (extra, expected) in [
        (
            "\n[environments.canary]\ndeployed = ['x']\n",
            "environments.canary: unknown stage (expected one of staging, production)",
        ),
        (
            "\n[environments.staging]\ndeployed = []\n",
            "environments.staging.deployed must be an array of nonempty strings",
        ),
        (
            "\n[environments.staging]\nevery_seconds = 30\n",
            "environments.staging.deployed must be an array of nonempty strings",
        ),
        (
            "\n[environments.production]\ndeployed = ['x', ' ']\n",
            "environments.production.deployed must be an array of nonempty strings",
        ),
        (
            "\n[environments.staging]\ndeployed = ['{{main_clone}}/probe']\n",
            "environments.staging.deployed[0] is the program to run and cannot be a template",
        ),
        (
            "\n[environments.staging]\ndeployed = ['x']\nevery_seconds = 14\n",
            "environments.staging.every_seconds must be at least 15",
        ),
        (
            "\n[environments.staging]\ndeployed = ['x']\ntimeout_seconds = 0\n",
            "environments.staging.timeout_seconds must be a positive integer",
        ),
        (
            "\n[environments.staging]\ndeployed = 'x --sha'\n",
            "environments.staging: ",
        ),
        (
            "\n[environments]\nstaging = 'x'\n",
            "environments.staging must be a table",
        ),
    ] {
        let error = load(extra).unwrap_err().to_string();
        assert!(error.contains(expected), "{extra}: {error}");
    }
}

#[test]
fn stage_deployed_rules_need_a_probed_stage_and_probes_shorten_the_verification_settle() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let load = |branch: &str, extra: &str| {
        let mut text = minimal_config(root.path());
        text.push_str(branch);
        text.push_str(extra);
        Config::from_raw(toml_value(&text), &options(root.path(), &home, &[]))
    };
    let due =
        "\n[[automations]]\nid='verify'\non='status.verification_due'\nrun='builtin:notify'\n";
    let staging = "\n[environments.staging]\ndeployed = ['x']\n";
    let production = "\n[environments.production]\ndeployed = ['x']\n";
    let promoted = "production='release'\nkeep_fresh=['release']\n";
    let settle =
        |branch: &str, extra: String| load(branch, &extra).unwrap().automations[0].settle_seconds;
    // No probe, or a stage that can come due without one: the old delay.
    assert_eq!(settle("", due.to_owned()), 600.0);
    assert_eq!(settle("", format!("{production}{due}")), 600.0);
    assert_eq!(settle(promoted, format!("{staging}{due}")), 600.0);
    // One probe is not both, with or without a production branch.
    assert_eq!(settle("", format!("{staging}{due}")), 600.0);
    assert_eq!(settle("", format!("{staging}{production}{due}")), 30.0);
    assert_eq!(
        settle(promoted, format!("{staging}{production}{due}")),
        30.0
    );
    assert_eq!(settle("", format!("{staging}{due}settle_seconds=5\n")), 5.0);

    let deployed = "\n[[automations]]\nid='released'\non='stage.deployed'\nrun='builtin:notify'\n";
    let rule = load("", &format!("{staging}{deployed}stage=' staging '\n"))
        .unwrap()
        .automations
        .remove(0);
    assert_eq!(rule.on, wt_config::AutomationTrigger::StageDeployed);
    assert_eq!(rule.stage, Some(wt_core::VerifyStage::Staging));
    assert_eq!(rule.settle_seconds, 10.0);
    for (extra, expected) in [
        (
            deployed.to_owned(),
            "on = \"stage.deployed\" requires stage = \"staging\" or \"production\"",
        ),
        (
            format!("{staging}{deployed}stage='canary'\n"),
            "automations[0].stage must be \"staging\" or \"production\" (got \"canary\")",
        ),
        (
            format!("{staging}{deployed}stage='production'\n"),
            "automations[0].stage \"production\" has no [environments.production] deploy probe",
        ),
        (
            format!("{staging}{due}stage='staging'\n"),
            "automations[0].stage only applies to on = \"stage.deployed\" (got \"status.verification_due\")",
        ),
    ] {
        let error = load("", &extra).unwrap_err().to_string();
        assert!(error.contains(expected), "{extra}: {error}");
    }
}
