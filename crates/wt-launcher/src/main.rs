use std::{
    env,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

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
    #[cfg(unix)]
    {
        Err(wt_launcher::exec(&paths, &args, now).into())
    }
    #[cfg(not(unix))]
    {
        let status = wt_launcher::launch(&paths, &args, now)?;
        Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1))
    }
}
