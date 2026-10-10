//! Test-only bridge for the native action smoke script. It exercises the real
//! ActionService start/duplicate/kill path without adding a user-facing CLI.

use std::{collections::BTreeMap, env, path::PathBuf, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_actions::{
    ActionRequest, ActionRunKind, ActionService, ActionServiceConfig, ActionServiceError,
};
use wt_platform::process::ProcessRunner;
use wt_tmux::{TmuxClient, TmuxServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = env::args_os().skip(1);
    let binary = PathBuf::from(args.next().expect("native wt binary"));
    let root = PathBuf::from(args.next().expect("fixture root"));
    let cwd = PathBuf::from(args.next().expect("working directory"));
    let socket = args
        .next()
        .expect("private tmux socket")
        .to_string_lossy()
        .into_owned();
    let global_config = PathBuf::from(args.next().expect("WT_CONFIG path"));
    let repo_config = PathBuf::from(args.next().expect("WT_REPO_CONFIG path"));

    let runner = ProcessRunner::default();
    let service = ActionService::new(ActionServiceConfig {
        log_dir: root.join("logs"),
        lock_dir: root.join("locks"),
        executable: binary,
        runner: runner.clone(),
        tmux: TmuxClient::new(runner, TmuxServer::named(socket)),
    });
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        "sleep 60 & echo $! > \"$1/child.pid\"; wait".into(),
        "action-fixture".into(),
        root.to_string_lossy().into_owned(),
    ];
    let request = ActionRequest {
        issue_status: None,
        action_key: "fixture-action".into(),
        slug: "fixture-action".into(),
        worktree_ref: None,
        action_id: "service-smoke".into(),
        action_name: "Action service fixture".into(),
        arg_history: None,
        prompt: "private fixture only".into(),
        kind: ActionRunKind::Shell,
        command,
        cwd,
        affects: Vec::new(),
        external: false,
        auto_fire_keys: Vec::new(),
        config_selectors: BTreeMap::from([
            (
                "WT_CONFIG".into(),
                global_config.to_string_lossy().into_owned(),
            ),
            (
                "WT_REPO_CONFIG".into(),
                repo_config.to_string_lossy().into_owned(),
            ),
        ]),
    };
    let cancellation = CancellationToken::new();
    let started = service.start(request.clone(), &cancellation).await?;
    let duplicate = service.start(request, &cancellation).await;
    if !matches!(duplicate, Err(ActionServiceError::AlreadyRunning(_))) {
        anyhow::bail!("duplicate action was not rejected: {duplicate:?}");
    }

    tokio::time::timeout(Duration::from_secs(8), async {
        while !root.join("child.pid").is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    if !service.kill("fixture-action", &cancellation).await? {
        anyhow::bail!("running action disappeared before kill");
    }
    println!(
        "{}\n{}\n{}",
        started.run_id,
        started.run_dir.display(),
        started.session
    );
    Ok(())
}
