use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let cache = args
        .next()
        .map(PathBuf::from)
        .ok_or("expected cache root")?;
    let home = args.next().map(PathBuf::from).ok_or("expected home")?;
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let path = wt_tmux::write_terminal_palette_config(&cache, &home)?;
    println!("{}", path.display());
    Ok(())
}
