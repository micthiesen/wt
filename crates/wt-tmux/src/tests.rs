use std::{path::PathBuf, time::Duration};

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use wt_platform::process::ProcessRunner;

use crate::{
    CreateSession, CreateWindow, OptionScope, PaneTarget, TmuxClient, TmuxServer, WindowTarget,
    exact_pane_target, exact_session_target, exact_window_target, parse_panes, parse_sessions,
    parse_windows, shell_quote,
};

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
    let result = tokio::time::timeout(Duration::from_secs(20), async {
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
        assert!(client.list_sessions(&cancellation).await?.is_empty());
        Ok::<(), crate::TmuxError>(())
    })
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
