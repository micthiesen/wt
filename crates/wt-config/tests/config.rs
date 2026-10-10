use std::fs;
use std::path::Path;
use wt_config::{
    ActivityPane, BackendKind, Config, ConfigError, EffectTag, HarnessId, LoadOptions,
    NamingHarness, ReviewBotMode, UiSort,
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
        "remote": config.remote.as_ref().map(|remote| json!({ "host": remote.host, "label": remote.label, "wtPath": remote.wt_path })),
        "naming": config.naming.as_ref().map(|naming| json!({ "autoRename": naming.auto_rename,
            "harness": format!("{:?}", naming.harness).to_lowercase(), "models": naming.models,
            "reasoningEffort": format!("{:?}", naming.reasoning_effort).to_lowercase(),
            "maxInputTokens": number(naming.max_input_tokens), "timeoutMs": number(naming.timeout_ms) })),
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
                wt_config::AutomationTrigger::WtMerged => "wt.merged",
                wt_config::AutomationTrigger::WtCreated => "wt.created",
                wt_config::AutomationTrigger::StackParentMerged => "stack.parent_merged",
                wt_config::AutomationTrigger::StatusNeedsHuman => "status.needs_human",
                wt_config::AutomationTrigger::StatusNeedsTesting => "status.needs_testing",
                wt_config::AutomationTrigger::StatusReady => "status.ready",
                wt_config::AutomationTrigger::StatusVerificationOverdue => "status.verification_overdue",
                wt_config::AutomationTrigger::BranchAdvanced => "branch.advanced",
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
    let unsupported = format!("{base}\n[harness]\nprimary = 'pi'\n");
    assert!(Config::from_raw(toml_value(&unsupported), &opts).is_err());
    let all_hidden = format!("{base}\n[harness]\nhidden = ['claude', 'codex', 'opencode']\n");
    assert!(Config::from_raw(toml_value(&all_hidden), &opts).is_err());
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
fn action_and_automation_aliases_normalize_with_defaults_and_constraints() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut text = minimal_config(root.path());
    text.push_str(
        "\nbase='main'\nkeep_fresh=['release']\n\
         [[actions]]\nid='triage'\nname='Triage issue'\nshell='echo {{issue}}'\n\
         affects=['issue','issue']\nrequires=['issue.tracker','pr.ready']\n\
         issue_status='Doing'\nkey='t'\ngroup='Issues'\nexternal=true\n\
         arg_prompt='Issue title'\nlabel_extract='^([A-Z]+-\\d+)'\n\
         [[automations]]\nid='legacy-review'\non='rabbit.unresolved'\nrun='triage'\n\
         [[automations]]\nid='needs-human'\non='status.needs-human'\nrun='builtin:notify'\n\
         [[automations]]\nid='advance-release'\non='branch.advanced'\nrun='builtin:notify'\nbranch=' release '\n\
         [[automations]]\nid='overdue'\non='status.verification_overdue'\nrun='builtin:notify'\nafter_days=4\n",
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

    assert_eq!(config.automations.len(), 4);
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
    assert_eq!(config.remote.as_ref().unwrap().label, "worker");
    assert_eq!(config.remote.as_ref().unwrap().wt_path, "~/.wt/bin/wt");
    assert_eq!(
        config.naming.as_ref().unwrap().harness,
        NamingHarness::Codex
    );
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
