//! The clock port (spec §5.5): the **only** wall-clock read in the crate.
//!
//! Every timestamp clog assigns — `recorded_at`, `Situation.as_of`, the
//! `now_ms` handed to scoring — comes from here, so a `Manual` clock makes
//! the whole engine a pure function of its inputs (INV-10). Tests always
//! run `Manual`; hosts default to `System`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::ClockMode;

/// clog's clock: either the system clock or a manually driven counter.
///
/// Cloning a `Manual` clock shares the same counter, so the handle and the
/// writer thread always agree on the current time.
#[derive(Clone)]
pub(crate) enum Clock {
    /// Reads `SystemTime::now()`.
    System,
    /// Reads a shared counter advanced only by `Clog::advance`.
    Manual(Arc<AtomicU64>),
}

impl Clock {
    /// Builds the clock a `ClockMode` asks for. A `Manual` clock starts at 0.
    pub(crate) fn new(mode: ClockMode) -> Clock {
        match mode {
            ClockMode::System => Clock::System,
            ClockMode::Manual => Clock::Manual(Arc::new(AtomicU64::new(0))),
        }
    }

    /// The current time in milliseconds since the Unix epoch.
    ///
    /// A `System` clock reading before the epoch (only possible with a
    /// grossly misconfigured host clock) reads as 0 rather than panicking:
    /// clog never lets a clock read fail a write.
    pub(crate) fn now_ms(&self) -> u64 {
        match self {
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            Clock::Manual(counter) => counter.load(Ordering::SeqCst),
        }
    }

    /// Advances a `Manual` clock by `ms`, returning the new reading.
    /// Saturating: a manual clock never wraps back into the past.
    ///
    /// A no-op on a `System` clock — `Clog::advance` rejects that case with
    /// `ManualClockRequired` before ever reaching here.
    pub(crate) fn advance(&self, ms: u64) -> u64 {
        match self {
            Clock::System => self.now_ms(),
            Clock::Manual(counter) => {
                // `fetch_update` rather than `fetch_add` so the saturation is
                // atomic too: two concurrent advances can never wrap.
                counter
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                        Some(now.saturating_add(ms))
                    })
                    .unwrap_or(0)
                    .saturating_add(ms)
            }
        }
    }

    /// Whether this is a `Manual` clock.
    pub(crate) fn is_manual(&self) -> bool {
        matches!(self, Clock::Manual(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_starts_at_zero_and_advances() {
        let c = Clock::new(ClockMode::Manual);
        assert!(c.is_manual());
        assert_eq!(c.now_ms(), 0);
        assert_eq!(c.advance(1_000), 1_000);
        assert_eq!(c.now_ms(), 1_000);
        assert_eq!(c.advance(500), 1_500);
    }

    #[test]
    fn manual_advance_saturates() {
        let c = Clock::new(ClockMode::Manual);
        c.advance(u64::MAX);
        assert_eq!(c.advance(10), u64::MAX);
    }

    #[test]
    fn manual_clone_shares_the_counter() {
        let a = Clock::new(ClockMode::Manual);
        let b = a.clone();
        a.advance(42);
        assert_eq!(b.now_ms(), 42);
    }

    #[test]
    fn system_clock_reads_wall_time() {
        let c = Clock::new(ClockMode::System);
        assert!(!c.is_manual());
        // 2020-01-01 in ms; any sane host clock is past this.
        assert!(c.now_ms() > 1_577_836_800_000);
    }
}
