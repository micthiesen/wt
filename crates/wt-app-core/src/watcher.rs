//! Platform watcher policy for wt's filesystem invalidation sources.
//!
//! macOS kqueue reacts to directory link-count changes by tearing down and
//! rebuilding descendant watches. That can close watched SQLite WAL/SHM files;
//! closing a SHM descriptor releases SQLite's process-wide POSIX locks and can
//! corrupt a concurrent writer. Use metadata-only polling there, and keep
//! `compare_contents(false)` so polling never opens those files. Other platforms
//! keep notify's native backend.

#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
pub type WtWatcher = notify::PollWatcher;
#[cfg(not(target_os = "macos"))]
pub type WtWatcher = notify::RecommendedWatcher;

pub fn new<F>(handler: F) -> notify::Result<WtWatcher>
where
    F: notify::EventHandler,
{
    #[cfg(target_os = "macos")]
    {
        notify::PollWatcher::new(
            handler,
            notify::Config::default()
                .with_poll_interval(Duration::from_secs(1))
                .with_compare_contents(false),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        notify::recommended_watcher(handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::Watcher;

    #[test]
    fn platform_watcher_uses_bounded_polling_on_macos() {
        #[cfg(target_os = "macos")]
        assert_eq!(WtWatcher::kind(), notify::WatcherKind::PollWatcher);

        #[cfg(not(target_os = "macos"))]
        assert_eq!(WtWatcher::kind(), notify::RecommendedWatcher::kind());
    }
}
