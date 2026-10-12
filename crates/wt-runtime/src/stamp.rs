//! Observation stamps. A stamp names the moment a fetch started on one host's
//! process clock. Stamps from different processes never compare, so a value
//! observed elsewhere can neither prove nor disprove ordering here.
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// Reserved epoch for observations whose start time is unknown, such as
/// restored cache entries. They never order against anything.
const UNORDERED: u64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp {
    epoch: u64,
    offset_ms: u64,
}

struct ProcessClock {
    epoch: u64,
    origin: Instant,
}

fn clock() -> &'static ProcessClock {
    static CLOCK: OnceLock<ProcessClock> = OnceLock::new();
    CLOCK.get_or_init(|| {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        // Distinct per process even when two start within one clock tick.
        let epoch = (wall ^ (u64::from(std::process::id()) << 40)) | 1;
        ProcessClock {
            epoch,
            origin: Instant::now(),
        }
    })
}

impl Stamp {
    /// The current moment on this process clock.
    pub fn now() -> Self {
        Self::at(Instant::now())
    }

    /// A moment on this process clock, e.g. the start of a fetch.
    pub fn at(instant: Instant) -> Self {
        let clock = clock();
        Self {
            epoch: clock.epoch,
            offset_ms: instant.saturating_duration_since(clock.origin).as_millis() as u64,
        }
    }

    /// An observation whose start time is unknown. It can be displayed and
    /// aged by other means, but never settles a pending mutation.
    pub const fn unordered() -> Self {
        Self {
            epoch: UNORDERED,
            offset_ms: 0,
        }
    }

    pub fn is_ordered(&self) -> bool {
        self.epoch != UNORDERED
    }

    /// `Some(true)` only when both stamps come from the same process clock and
    /// `self` is not earlier. `None` means the order is unknowable.
    pub fn at_or_after(&self, other: &Stamp) -> Option<bool> {
        if !self.is_ordered() || !other.is_ordered() || self.epoch != other.epoch {
            return None;
        }
        Some(self.offset_ms >= other.offset_ms)
    }

    /// The earlier of two observations. Unknown order is never resolved in
    /// favor of the newer one: a merge with an unordered or foreign stamp is
    /// unordered.
    pub fn earliest(self, other: Stamp) -> Stamp {
        match self.at_or_after(&other) {
            Some(true) => other,
            Some(false) => self,
            None => Stamp::unordered(),
        }
    }

    /// The local instant for a stamp from this process, if it is one.
    pub fn local_instant(&self) -> Option<Instant> {
        let clock = clock();
        (self.epoch == clock.epoch && self.is_ordered())
            .then(|| clock.origin + std::time::Duration::from_millis(self.offset_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn stamps_order_only_within_one_process_clock() {
        let early = Stamp::now();
        tokio::time::advance(Duration::from_millis(5)).await;
        let late = Stamp::now();
        assert_eq!(late.at_or_after(&early), Some(true));
        assert_eq!(early.at_or_after(&late), Some(false));
        let foreign = Stamp {
            epoch: late.epoch ^ 2,
            offset_ms: late.offset_ms + 1000,
        };
        assert_eq!(foreign.at_or_after(&early), None);
        assert_eq!(Stamp::unordered().at_or_after(&early), None);
        assert_eq!(early.earliest(late), early);
        assert!(!foreign.earliest(late).is_ordered());
        assert!(late.local_instant().is_some());
        assert!(foreign.local_instant().is_none());
    }
}
