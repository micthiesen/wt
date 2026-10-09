//! Typed configuration loading for wt.
//!
//! Loading is explicit: callers choose the working directory, home directory,
//! and environment snapshot. The crate never reads process-global state unless
//! `LoadOptions::default()` is requested.

mod discovery;
mod error;
mod load;
mod schema;

pub use error::ConfigError;
pub use load::LoadOptions;
pub use schema::*;

/// The stable namespace used for repository caches, state and tmux sockets.
/// `config_path` may refer to a config that has not been created yet.
pub fn repository_namespace(
    config_path: &std::path::Path,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> String {
    discovery::repository_namespace(config_path, home, cwd)
}
