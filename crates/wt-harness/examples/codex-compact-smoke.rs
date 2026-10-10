use std::{env, path::PathBuf, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_harness::{CodexMessageOutcome, CodexMessageTarget, CodexMessenger, CodexPaths};
use wt_platform::process::ProcessRunner;
use wt_tmux::{TmuxClient, TmuxServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let home = PathBuf::from(args.next().ok_or("expected isolated HOME")?);
    let codex_home = PathBuf::from(args.next().ok_or("expected isolated CODEX_HOME")?);
    let cache = PathBuf::from(args.next().ok_or("expected isolated cache directory")?);
    let socket = PathBuf::from(args.next().ok_or("expected private tmux socket")?);
    let cwd = PathBuf::from(args.next().ok_or("expected session cwd")?);
    if args.next().is_some() {
        return Err("too many arguments".into());
    }

    let tmux = TmuxClient::new(
        ProcessRunner::default(),
        TmuxServer::at(socket)
            .with_cwd(&home)
            .with_config_file("/dev/null"),
    );
    let paths = CodexPaths::new(&home, cache).with_codex_home(codex_home);
    let mut messenger = CodexMessenger::new(paths, ProcessRunner::default(), tmux);
    let target = CodexMessageTarget {
        slug: "manager".into(),
        cwd,
        managed_name: None,
        text: "/compact".into(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        messenger.send_target(&target, &CancellationToken::new()),
    )
    .await??;
    match outcome {
        CodexMessageOutcome::Terminal {
            cold_started: false,
            delivered: None,
            reason,
        } => println!("terminal-fallback: {reason}"),
        other => return Err(format!("expected native terminal fallback, got {other:?}").into()),
    }
    Ok(())
}
