use std::{path::PathBuf, time::Duration};

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use wt_platform::process::ProcessRunner;

use crate::{
    CreateSession, CreateWindow, OptionScope, PaneTarget, SESSION_SOURCE, TmuxClient, TmuxServer,
    WindowTarget, exact_pane_target, exact_session_target, exact_window_target, parse_panes,
    parse_sessions, parse_windows, save_terminal_palette, shell_quote, terminal_palette,
    terminal_palette_config, write_terminal_palette_config,
};

#[test]
fn server_shutdown_errors_do_not_prove_session_absence() {
    assert!(crate::tmux_server_definitely_absent(
        "no server running on /tmp/test.sock"
    ));
    assert!(crate::tmux_server_definitely_absent(
        "error connecting to /tmp/test.sock (No such file or directory)"
    ));
    for error in [
        "server exited unexpectedly",
        "Permission denied",
        "Connection refused",
    ] {
        assert!(!crate::tmux_server_definitely_absent(error));
    }
}

#[test]
fn parsers_accept_empty_output_and_keep_unusual_names() {
    assert!(parse_sessions("").unwrap().is_empty());
    assert!(parse_windows("").unwrap().is_empty());
    assert!(parse_panes("").unwrap().is_empty());

    let sep = ':';
    let session = format!("space ; '$ Ω{sep}$7{sep}123{sep}0{sep}1{sep}thread-1\n");
    let parsed = parse_sessions(&session).unwrap();
    assert_eq!(parsed[0].name, "space ; '$ Ω");
    assert_eq!(parsed[0].harness_session_id.as_deref(), Some("thread-1"));

    let window = format!("@4{sep}0{sep}1{sep}2{sep}window: Ω\n");
    assert_eq!(parse_windows(&window).unwrap()[0].name, "window: Ω");

    let pane = format!(
        "%9{sep}session{sep}@4{sep}0{sep}1{sep}1{sep}55{sep}80{sep}24{sep}0{sep}/tmp/a:b c\n"
    );
    let parsed = parse_panes(&pane).unwrap();
    assert_eq!(parsed[0].id, "%9");
    assert_eq!(parsed[0].current_path, PathBuf::from("/tmp/a:b c"));
}

#[test]
fn exact_targets_and_shell_quoting_are_structural() {
    assert_eq!(exact_session_target("a weird;name"), "=a weird;name");
    assert_eq!(exact_pane_target("a weird;name"), "=a weird;name:");
    assert_eq!(exact_window_target("a weird;name"), "=a weird;name:");
    assert_eq!(exact_window_target("session:2"), "=session:2");
    assert_eq!(exact_window_target("@4"), "@4");
    assert_eq!(PaneTarget::active_session_pane("demo").as_str(), "=demo:");
    assert_eq!(shell_quote("a'b $(touch nope)"), "'a'\\''b $(touch nope)'");
}

#[test]
fn production_palette_config_uses_only_valid_observations() {
    let colors = serde_json::json!({
        "defaultForeground": "#CDd6F4",
        "defaultBackground": "#1E1E2E",
        "futureField": {"keptByCaller": true}
    });
    assert_eq!(
        terminal_palette(&colors).unwrap().default_foreground,
        "#cdd6f4"
    );
    assert_eq!(
        terminal_palette_config(&colors),
        "set -g window-style 'fg=#cdd6f4,bg=#1e1e2e'\nset -g window-active-style 'fg=#cdd6f4,bg=#1e1e2e'\n"
    );
    assert!(
        terminal_palette_config(&serde_json::json!({"defaultBackground":"#000000"})).is_empty()
    );
    assert!(
        terminal_palette_config(&serde_json::json!({
            "defaultForeground":"#zzzzzz",
            "defaultBackground":"#000000"
        }))
        .is_empty()
    );

    let temp = tempdir().unwrap();
    let cache = temp.path().join("cache");
    let home = temp.path().join("home with spaces");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        cache.join("terminal-palette.json"),
        serde_json::to_vec(&colors).unwrap(),
    )
    .unwrap();
    let config = write_terminal_palette_config(&cache, &home).unwrap();
    let rendered = std::fs::read_to_string(&config).unwrap();
    assert!(rendered.starts_with("source-file -q '"));
    assert!(rendered.contains(&home.to_string_lossy().to_string()));
    assert!(rendered.contains("window-active-style 'fg=#cdd6f4,bg=#1e1e2e'"));

    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::named("isolated").with_config_file(&config),
    );
    assert!(
        client
            .create_session_args(&CreateSession {
                name: "palette".into(),
                cwd: home,
                command: vec!["cat".into()],
                width: None,
                height: None,
            })
            .iter()
            .any(|arg| arg == config.as_os_str())
    );
}

#[test]
fn palette_observations_are_atomic_validated_and_preserve_last_known_value() {
    let temp = tempdir().unwrap();
    let cache = temp.path().join("cache");
    assert!(save_terminal_palette(&cache, "#CDD6F4", "#1E1E2E").unwrap());
    let path = cache.join("terminal-palette.json");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r##"{"defaultForeground":"#cdd6f4","defaultBackground":"#1e1e2e"}"##
    );
    assert!(save_terminal_palette(&cache, "rgb:cdcd/d6d6/f4f4", "rgb:1e1e/1e1e/2e2e").unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r##"{"defaultForeground":"#cdd6f4","defaultBackground":"#1e1e2e"}"##
    );
    assert!(!save_terminal_palette(&cache, "default", "#000000").unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r##"{"defaultForeground":"#cdd6f4","defaultBackground":"#1e1e2e"}"##
    );
    assert_eq!(
        std::fs::read_dir(&cache).unwrap().count(),
        1,
        "atomic temporary files should be cleaned"
    );
}

#[test]
fn create_and_attach_commands_keep_user_values_in_argv() {
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at("/tmp/wt-tmux-test.sock").with_config_file("/dev/null"),
    );
    let session = CreateSession {
        name: "feature ; x".to_owned(),
        cwd: PathBuf::from("/tmp/dir with spaces"),
        command: vec![
            "printf".to_owned(),
            "%s\n".to_owned(),
            "$(touch /tmp/nope);'".to_owned(),
        ],
        width: Some(80),
        height: Some(24),
    };
    let args = client.create_session_args(&session);
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        args[0..4],
        ["-S", "/tmp/wt-tmux-test.sock", "-f", "/dev/null"]
    );
    assert_eq!(args[4], "new-session");
    assert_eq!(args[6], "-s");
    assert_eq!(args[7], "feature ; x");
    assert!(args.last().unwrap().contains("'$(touch /tmp/nope);'\\'''"));
    let attach = client.attach_session_args("feature ; x");
    assert_eq!(attach.last().unwrap(), "=feature ; x");
}

#[test]
fn session_environment_is_per_client_and_kept_in_session_argv() {
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at("/tmp/wt-tmux-test.sock").with_config_file("/dev/null"),
    )
    .with_session_environment([
        ("PATH".into(), "/private/bin:/usr/bin:/bin".into()),
        ("WT_CONFIG".into(), "/private/config.toml".into()),
        ("WT_REPO_CONFIG".into(), String::new()),
    ]);
    let args = client
        .create_session_args(&CreateSession {
            name: "env".into(),
            cwd: PathBuf::from("/tmp"),
            command: vec!["sh".into(), "-c".into(), "true".into()],
            width: None,
            height: None,
        })
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        &args[4..12],
        [
            "new-session",
            "-d",
            "-s",
            "env",
            "-c",
            "/tmp",
            "-e",
            "PATH=/private/bin:/usr/bin:/bin"
        ]
    );
    assert_eq!(
        &args[12..],
        [
            "-e",
            "WT_CONFIG=/private/config.toml",
            "-e",
            "WT_REPO_CONFIG=",
            "'/usr/bin/env' 'PATH=/private/bin:/usr/bin:/bin' 'WT_CONFIG=/private/config.toml' 'WT_REPO_CONFIG=' 'sh' '-c' 'true'"
        ]
    );
}

#[cfg(unix)]
#[test]
fn session_source_file_supplies_secrets_without_argv() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("secrets.sh");
    std::fs::write(&source, "export WT_TEST_SECRET='s3cret'\n").unwrap();
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at("/tmp/wt-tmux-test.sock").with_config_file("/dev/null"),
    )
    .with_session_environment([
        ("PATH".into(), "/usr/bin:/bin".into()),
        (SESSION_SOURCE.into(), source.to_string_lossy().into_owned()),
    ]);
    let args = client.create_session_args(&CreateSession {
        name: "secret".into(),
        cwd: PathBuf::from("/tmp"),
        command: vec![
            "sh".into(),
            "-c".into(),
            "printf %s \"$WT_TEST_SECRET\"".into(),
        ],
        width: None,
        height: None,
    });
    let command = args.last().unwrap().to_string_lossy().into_owned();
    assert!(
        !args
            .iter()
            .any(|arg| arg.to_string_lossy().contains("s3cret"))
    );
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &command])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "s3cret");
}

#[cfg(unix)]
#[tokio::test]
async fn distinct_session_environments_work_on_one_existing_server() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempdir().unwrap();
    let socket = temp.path().join("shared.sock");
    let cancellation = CancellationToken::new();
    let base = TmuxServer::at(&socket)
        .with_cwd(temp.path())
        .with_config_file("/dev/null");
    let mut sessions = Vec::new();

    for name in ["one", "two"] {
        let bin = temp.path().join(name).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let wt = bin.join("wt");
        std::fs::write(&wt, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o700)).unwrap();
        let observed = temp.path().join(format!("{name}.txt"));
        let path_result = temp.path().join(format!("{name}.path"));
        let client = TmuxClient::new(ProcessRunner::default(), base.clone())
            .with_session_environment([
                ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
                (
                    "WT_CONFIG".into(),
                    temp.path()
                        .join(format!("{name}.toml"))
                        .display()
                        .to_string(),
                ),
                (
                    "WT_REPO_CONFIG".into(),
                    temp.path()
                        .join(format!("{name}.wt.toml"))
                        .display()
                        .to_string(),
                ),
            ]);
        let script = "command -v wt > \"$1\"; wt > /dev/null; printf '%s\\n%s\\n' \"$WT_CONFIG\" \"$WT_REPO_CONFIG\" > \"$2.tmp\"; mv \"$2.tmp\" \"$2\"; sleep 30";
        let session = CreateSession {
            name: name.into(),
            cwd: temp.path().to_path_buf(),
            command: vec![
                "sh".into(),
                "-c".into(),
                script.into(),
                "_".into(),
                path_result.display().to_string(),
                observed.display().to_string(),
            ],
            width: None,
            height: None,
        };
        client
            .create_session(&session, &cancellation)
            .await
            .unwrap();
        sessions.push((client, name.to_owned(), wt, path_result, observed));
    }

    for _ in 0..100 {
        if sessions
            .iter()
            .all(|(_, _, _, path, observed)| path.exists() && observed.exists())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let observed_values = sessions
        .iter()
        .map(|(_, name, wt, path_result, observed)| {
            (
                name.clone(),
                wt.display().to_string(),
                std::fs::read_to_string(path_result),
                std::fs::read_to_string(observed),
            )
        })
        .collect::<Vec<_>>();
    for (client, name, _, _, _) in sessions {
        client.kill_session(&name, &cancellation).await.unwrap();
    }
    for (name, expected_wt, path_result, observed) in observed_values {
        assert_eq!(
            path_result.unwrap().trim(),
            expected_wt,
            "{name} must resolve and execute wt from its session PATH"
        );
        let values = observed.unwrap();
        assert!(values.contains(&format!("/{name}.toml\n")));
        assert!(values.contains(&format!("/{name}.wt.toml\n")));
    }
}

#[tokio::test]
async fn isolated_server_inventory_paste_options_rename_resize_and_kill() {
    let temp = tempdir().unwrap();
    let socket = temp.path().join("server.sock");
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at(&socket)
            .with_cwd(temp.path())
            .with_config_file("/dev/null"),
    );
    let cancellation = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        Box::pin(async {
            assert!(client.list_sessions(&cancellation).await?.is_empty());
            let session = CreateSession {
                name: "unusual ; ' session".to_owned(),
                cwd: temp.path().to_path_buf(),
                command: vec!["cat".into()],
                width: Some(80),
                height: Some(24),
            };
            client.create_session(&session, &cancellation).await?;
            assert!(client.session_exists(&session.name, &cancellation).await?);
            let sessions = client.list_sessions(&cancellation).await?;
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].name, session.name);

            let windows = client.list_windows(&session.name, &cancellation).await?;
            assert_eq!(windows.len(), 1);
            assert!(windows[0].active);
            client
                .set_option(
                    &OptionScope::Window(WindowTarget::id(windows[0].id.clone())),
                    "@wt-window-option",
                    Some("window value"),
                    &cancellation,
                )
                .await?;
            assert_eq!(
                client
                    .get_option(
                        &OptionScope::Window(WindowTarget::id(windows[0].id.clone())),
                        "@wt-window-option",
                        &cancellation,
                    )
                    .await?,
                Some("window value".to_owned())
            );
            let panes = client.list_panes(&session.name, &cancellation).await?;
            assert_eq!(panes.len(), 1);
            assert!(panes[0].active);
            let pane = PaneTarget::id(panes[0].id.clone());

            client.resize_pane(&pane, 70, 20, &cancellation).await?;
            client
                .set_option(
                    &OptionScope::Pane(PaneTarget::id(panes[0].id.clone())),
                    "@wt-test-option",
                    Some("value with spaces"),
                    &cancellation,
                )
                .await?;
            assert_eq!(
                client
                    .get_option(
                        &OptionScope::Pane(PaneTarget::id(panes[0].id.clone())),
                        "@wt-test-option",
                        &cancellation,
                    )
                    .await?,
                Some("value with spaces".to_owned())
            );
            client
                .set_option(
                    &OptionScope::Pane(PaneTarget::id(panes[0].id.clone())),
                    "@wt-test-option",
                    Some(""),
                    &cancellation,
                )
                .await?;
            assert_eq!(
                client
                    .get_option(
                        &OptionScope::Pane(PaneTarget::id(panes[0].id.clone())),
                        "@wt-test-option",
                        &cancellation,
                    )
                    .await?,
                Some(String::new())
            );
            client
                .set_option(
                    &OptionScope::Pane(PaneTarget::id(panes[0].id.clone())),
                    "@wt-test-option",
                    None,
                    &cancellation,
                )
                .await?;
            assert_eq!(
                client
                    .get_option(
                        &OptionScope::Pane(PaneTarget::id(panes[0].id.clone())),
                        "@wt-test-option",
                        &cancellation,
                    )
                    .await?,
                None
            );

            let poison = temp.path().join("should-not-run");
            let text = "literal $HOME; $(touch should-not-run) Ω";
            client.send_literal(&pane, text, &cancellation).await?;
            let capture = client.capture_pane(&pane, Some(20), &cancellation).await?;
            assert!(capture.contains(text), "captured pane was {capture:?}");
            assert_eq!(capture.matches("literal $HOME").count(), 1);
            assert!(!poison.exists(), "literal pane text executed as a command");
            assert_no_injection_buffers(&client, &cancellation).await?;

            let paste_error = client
                .send_literal(&PaneTarget::id("%999999"), "invalid target", &cancellation)
                .await;
            assert!(paste_error.is_err());
            assert_no_injection_buffers(&client, &cancellation).await?;

            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert!(
                client
                    .send_literal(&pane, "cancelled load", &cancelled)
                    .await
                    .is_err()
            );
            assert_no_injection_buffers(&client, &cancellation).await?;

            client
                .create_window(
                    &CreateWindow {
                        session: session.name.clone(),
                        name: Some("second window".to_owned()),
                        cwd: Some(temp.path().to_path_buf()),
                        command: vec!["cat".to_owned()],
                    },
                    &cancellation,
                )
                .await?;
            let windows = client.list_windows(&session.name, &cancellation).await?;
            assert_eq!(windows.len(), 2);
            let second_window = WindowTarget::id(windows[1].id.clone());
            client.select_window(&second_window, &cancellation).await?;
            client
                .rename_window(
                    &WindowTarget::active_session_window(&session.name),
                    "renamed: window",
                    &cancellation,
                )
                .await?;
            let windows = client.list_windows(&session.name, &cancellation).await?;
            assert_eq!(
                windows
                    .iter()
                    .find(|window| window.id == windows[1].id)
                    .unwrap()
                    .name,
                "renamed: window"
            );
            client.kill_window(&second_window, &cancellation).await?;
            assert_eq!(
                client
                    .list_windows(&session.name, &cancellation)
                    .await?
                    .len(),
                1
            );
            client
                .rename_session(&session.name, "renamed session", &cancellation)
                .await?;
            assert!(!client.session_exists(&session.name, &cancellation).await?);
            assert!(
                client
                    .session_exists("renamed session", &cancellation)
                    .await?
            );
            let old_id = client.list_sessions(&cancellation).await?[0].id.clone();
            let anchor = CreateSession {
                name: "anchor".into(),
                ..session.clone()
            };
            client.create_session(&anchor, &cancellation).await?;
            assert!(
                client
                    .kill_session("renamed session", &cancellation)
                    .await?
            );
            assert!(
                !client
                    .session_exists("renamed session", &cancellation)
                    .await?
            );
            assert_eq!(client.list_sessions(&cancellation).await?.len(), 1);
            let replacement = CreateSession {
                name: "renamed session".into(),
                ..session.clone()
            };
            client.create_session(&replacement, &cancellation).await?;
            let replacement_id = client
                .list_sessions(&cancellation)
                .await?
                .into_iter()
                .find(|s| s.name == replacement.name)
                .unwrap()
                .id;
            assert_ne!(replacement_id, old_id);
            assert!(!client.kill_session_id(&old_id, &cancellation).await?);
            assert!(
                client
                    .session_exists(&replacement.name, &cancellation)
                    .await?
            );
            assert!(
                client
                    .kill_session_id(&replacement_id, &cancellation)
                    .await?
            );
            assert!(
                client
                    .kill_session_id("renamed session", &cancellation)
                    .await
                    .is_err()
            );
            client.kill_session(&anchor.name, &cancellation).await?;
            Ok::<(), crate::TmuxError>(())
        }),
    )
    .await;
    let cleanup = client.kill_server(&CancellationToken::new()).await;
    cleanup.unwrap();
    result
        .expect("isolated tmux integration timed out")
        .unwrap();
}

async fn assert_no_injection_buffers(
    client: &TmuxClient,
    cancellation: &CancellationToken,
) -> Result<(), crate::TmuxError> {
    let output = client
        .run(
            "list-buffers",
            ["list-buffers", "-F", "#{buffer_name}"],
            None,
            cancellation,
        )
        .await?;
    assert!(output.status.success());
    assert!(
        !output
            .stdout_text()
            .lines()
            .any(|name| name.starts_with("wt-tmux-")),
        "a temporary wt paste buffer leaked"
    );
    Ok(())
}

#[tokio::test]
async fn generated_terminal_config_reloads_navigation_without_replacing_sessions() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("empty home");
    std::fs::create_dir_all(&home).unwrap();
    let cache = temp.path().join("cache");
    let config = crate::write_terminal_config(&cache, &home, None).unwrap();
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at(temp.path().join("tmux.sock")).with_config_file(&config),
    );
    let cancel = CancellationToken::new();
    let result = async {
        client
            .create_session(
                &CreateSession {
                    name: "retained".into(),
                    cwd: home.clone(),
                    command: vec!["cat".into()],
                    width: Some(80),
                    height: Some(24),
                },
                &cancel,
            )
            .await?;
        let original = client.list_sessions(&cancel).await?;
        let keys = client
            .run("list-keys", ["list-keys", "-T", "root"], None, &cancel)
            .await?;
        assert!(keys.status.success(), "{}", keys.stderr_text());
        for key in ["F10", "F11", "F12"] {
            assert!(
                keys.stdout_text()
                    .lines()
                    .any(|line| line.contains(key) && line.ends_with("detach-client")),
                "{}",
                keys.stdout_text()
            );
        }
        let options = client
            .run(
                "show-options",
                ["show-options", "-g", "status"],
                None,
                &cancel,
            )
            .await?;
        assert_eq!(options.stdout_text().trim(), "status off");
        crate::write_terminal_config(&cache, &home, Some("set -g status on\n")).unwrap();
        let args = client.attach_session_args("retained");
        let source = args.iter().position(|arg| arg == "source-file").unwrap();
        let attach = args.iter().position(|arg| arg == "attach-session").unwrap();
        assert!(source < attach);
        let loaded = client
            .run(
                "source-file",
                ["source-file", args[source + 1].to_str().unwrap()],
                None,
                &cancel,
            )
            .await?;
        assert!(loaded.status.success(), "{}", loaded.stderr_text());
        let options = client
            .run(
                "show-options",
                ["show-options", "-g", "status"],
                None,
                &cancel,
            )
            .await?;
        assert_eq!(options.stdout_text().trim(), "status on");
        assert_eq!(client.list_sessions(&cancel).await?, original);
        Ok::<(), crate::TmuxError>(())
    }
    .await;
    client.kill_server(&cancel).await.unwrap();
    result.unwrap();
}

#[test]
fn empty_terminal_preamble_still_has_palette_and_board_return_keys() {
    let temp = tempdir().unwrap();
    save_terminal_palette(temp.path(), "#ffffff", "#000000").unwrap();
    let config = crate::write_terminal_config(temp.path(), temp.path(), Some("")).unwrap();
    let content = std::fs::read_to_string(config).unwrap();
    assert!(!content.contains("set -g mouse"));
    assert!(content.contains("window-active-style 'fg=#ffffff,bg=#000000'"));
    assert!(content.contains("bind-key -n F12 detach-client"));
    assert!(content.contains("remain-on-exit-format"));
}

#[tokio::test]
async fn failed_commands_remain_visible_and_retry_without_killing_live_commands() {
    let temp = tempdir().unwrap();
    let config =
        crate::write_terminal_config(temp.path(), temp.path(), Some("set -g remain-on-exit off"))
            .unwrap();
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at(temp.path().join("tmux.sock")).with_config_file(config),
    )
    .with_failure_retention();
    let cancel = CancellationToken::new();
    let result = async {
        client.create_session(&CreateSession {
            name: "failure".into(), cwd: temp.path().into(),
            command: vec!["sh".into(), "-c".into(), "if [ -f retried ]; then exec cat; else touch retried; echo bootstrap-failed; exit 23; fi".into()],
            width: Some(80), height: Some(24),
        }, &cancel).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if client.list_panes("failure", &cancel).await?.iter().any(|pane| pane.dead) { break; }
            assert!(tokio::time::Instant::now() < deadline, "failed pane was not retained");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let output = client.capture_pane(&PaneTarget::active_session_pane("failure"), Some(100), &cancel).await?;
        assert!(output.contains("bootstrap-failed"), "{output}");
        let retry = client.run("if-shell", ["if-shell", "-t", "=failure:", "-F", "#{pane_dead}", "respawn-pane -t =failure:", "send-keys -t =failure: Enter"], None, &cancel).await?;
        assert!(retry.status.success(), "{}", retry.stderr_text());
        let panes = client.list_panes("failure", &cancel).await?;
        assert!(!panes[0].dead);
        let pid = panes[0].pid;
        let enter = client.run("if-shell", ["if-shell", "-t", "=failure:", "-F", "#{pane_dead}", "respawn-pane -t =failure:", "send-keys -t =failure: Enter"], None, &cancel).await?;
        assert!(enter.status.success());
        assert_eq!(client.list_panes("failure", &cancel).await?[0].pid, pid);
        // A successful retry must clear its previous failure retention.
        client.send_keys(&PaneTarget::active_session_pane("failure"), &["C-d"], &cancel).await?;
        await_session_exit(&client, "failure", &cancel).await?;
        client.create_session(&CreateSession {
            name: "success".into(), cwd: temp.path().into(),
            command: vec!["true".into()], width: None, height: None,
        }, &cancel).await?;
        await_session_exit(&client, "success", &cancel).await?;
        let background = TmuxClient::new(ProcessRunner::default(), client.server().clone());
        background.create_session(&CreateSession {
            name: "background".into(), cwd: temp.path().into(),
            command: vec!["false".into()], width: None, height: None,
        }, &cancel).await?;
        await_session_exit(&background, "background", &cancel).await?;
        Ok::<(), crate::TmuxError>(())
    }.await;
    client.kill_server(&cancel).await.unwrap();
    result.unwrap();
}

async fn await_session_exit(
    client: &TmuxClient,
    name: &str,
    cancel: &CancellationToken,
) -> Result<(), crate::TmuxError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while client.session_exists(name, cancel).await? {
        if tokio::time::Instant::now() >= deadline {
            let target = exact_pane_target(name);
            let detail = client.run("list-panes", ["list-panes", "-t", &target, "-F", "pid=#{pane_pid} dead=#{pane_dead} status=#{pane_dead_status} command=#{pane_current_command}"], None, cancel).await?;
            let screen = client
                .capture_pane(&PaneTarget::active_session_pane(name), None, cancel)
                .await?;
            panic!(
                "session {name} unexpectedly retained: {}\n{screen}",
                detail.stdout_text()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

#[tokio::test]
async fn supervised_terminal_retains_signal_failures_and_preserves_quoted_arguments() {
    let temp = tempdir().unwrap();
    let client = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at(temp.path().join("tmux.sock")).with_config_file("/dev/null"),
    )
    .with_failure_retention();
    let cancel = CancellationToken::new();
    for (name, command, expected) in [
        ("exit", vec!["sh", "-c", "exit 27"], 27),
        ("signal", vec!["sh", "-c", "kill -TERM $$"], 143),
        ("interrupt", vec!["sh", "-c", "kill -INT $$"], 130),
        (
            "terminal",
            vec![
                "sh",
                "-c",
                "test -t 0 && test -t 1 && printf READY && exec cat",
            ],
            130,
        ),
        (
            "quoted",
            vec![
                "sh",
                "-c",
                "test \"$1\" = 'spaces ; $(false)'",
                "sh",
                "spaces ; $(false)",
            ],
            0,
        ),
    ] {
        client
            .create_session(
                &CreateSession {
                    name: name.into(),
                    cwd: temp.path().into(),
                    command: command.into_iter().map(str::to_owned).collect(),
                    width: None,
                    height: None,
                },
                &cancel,
            )
            .await
            .unwrap();
        if name == "terminal" {
            let target = PaneTarget::active_session_pane(name);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !client
                .capture_pane(&target, Some(10), &cancel)
                .await
                .unwrap()
                .contains("READY")
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "foreground command lacks a TTY"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            client.send_keys(&target, &["C-c"], &cancel).await.unwrap();
        }
        if expected == 0 {
            await_session_exit(&client, name, &cancel).await.unwrap();
            continue;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = client
                .run(
                    "display-message",
                    [
                        "display-message",
                        "-p",
                        "-t",
                        &exact_pane_target(name),
                        "#{pane_dead}:#{@wt-terminal-exit-status}",
                    ],
                    None,
                    &cancel,
                )
                .await
                .unwrap();
            if status.stdout_text().trim() == format!("1:{expected}") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{name}: expected {expected}, got {}",
                status.stdout_text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    client.kill_server(&cancel).await.unwrap();
}
