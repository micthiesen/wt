use std::{path::PathBuf, sync::Arc};

use tokio_util::sync::CancellationToken;
use wt_config::Config;
use wt_platform::process::ProcessRunner;
use wt_vcs::GitRepository;

use crate::database::Database;

/// Explicit application services shared by CLI handlers and TUI actions.
/// Presentation never owns this context or calls these services during render.
#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<Config>,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub database: Database,
    pub repository: Arc<GitRepository>,
    pub processes: ProcessRunner,
    pub cancellation: CancellationToken,
    /// Section moves this process made, so the attention feed can tell the
    /// user's own moves from moves made elsewhere.
    pub section_writes: SectionWrites,
}

/// Section assignments this process wrote and has not yet seen come back
/// through the state source. Bounded and short-lived: an unconsumed entry
/// expires, so a missed observation cannot hide a later outside move.
#[derive(Clone, Default)]
pub struct SectionWrites(Arc<std::sync::Mutex<Vec<SectionWrite>>>);

struct SectionWrite {
    key: String,
    section: Option<String>,
    at: std::time::Instant,
}

const SECTION_WRITE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const SECTION_WRITE_CAP: usize = 512;

impl SectionWrites {
    pub fn record(&self, keys: impl IntoIterator<Item = String>, section: Option<&str>) {
        let mut writes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        writes.retain(|write| now.duration_since(write.at) < SECTION_WRITE_TTL);
        writes.extend(keys.into_iter().map(|key| SectionWrite {
            key,
            section: section.map(str::to_owned),
            at: now,
        }));
        if writes.len() > SECTION_WRITE_CAP {
            let excess = writes.len() - SECTION_WRITE_CAP;
            writes.drain(..excess);
        }
    }

    /// Whether `key` landing in `section` is one of this process's writes;
    /// a match is consumed.
    pub fn consume(&self, key: &str, section: Option<&str>) -> bool {
        let mut writes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        writes.retain(|write| now.duration_since(write.at) < SECTION_WRITE_TTL);
        let matches = |write: &SectionWrite| {
            write.key == key
                && match (write.section.as_deref(), section) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, None) => true,
                    _ => false,
                }
        };
        match writes.iter().position(matches) {
            Some(index) => {
                writes.remove(index);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SectionWrites;

    #[test]
    fn a_recorded_section_write_is_consumed_once() {
        let writes = SectionWrites::default();
        writes.record(["a".to_owned()], Some("To Merge"));
        assert!(!writes.consume("a", None));
        assert!(writes.consume("a", Some("to merge")));
        assert!(
            !writes.consume("a", Some("To Merge")),
            "a second move is someone else's"
        );
        writes.record(["b".to_owned()], None);
        assert!(writes.consume("b", None));
    }
}
