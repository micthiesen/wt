use std::{
    env,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use wt_launcher::launch;
use wt_update::InstallPaths;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("wt launcher: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    // macOS may retain the invoked PATH symlink in current_exe(). Resolve it
    // before locating state beside the installed launcher.
    let executable = env::current_exe()?.canonicalize()?;
    let root = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("launcher must live at <install-root>/bin/wt")?;
    let paths = InstallPaths::new(root.to_owned())?;
    let args: Vec<_> = env::args_os().skip(1).collect();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let status = launch(&paths, &args, now)?;
    if let Some(code) = status.code() {
        Ok(u8::try_from(code).unwrap_or(1))
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            Ok(status
                .signal()
                .and_then(|signal| u8::try_from(128 + signal).ok())
                .unwrap_or(1))
        }
        #[cfg(not(unix))]
        {
            Ok(1)
        }
    }
}
