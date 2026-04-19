//! Time abstraction.
//!
//! The sync state machine must be deterministic under tests. Every function
//! that reads wall-clock time takes a `Clock` instead of calling
//! `SystemTime::now()` directly. Production code passes `SystemClock`; tests
//! pass `MockClock`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Reads the current wall-clock time as milliseconds since the UNIX epoch.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Real-time clock backed by `SystemTime`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // A system clock set to before 1970 is a configuration error, not
        // something this layer tries to paper over. The u128 → u64 conversion
        // holds for ~584 million years; beyond that our grandchildren's
        // grandchildren can file a bug.
        let millis_u128 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX_EPOCH")
            .as_millis();
        u64::try_from(millis_u128).unwrap_or(u64::MAX)
    }
}

/// Deterministic clock for tests. Starts at `0`, advances only when asked.
#[derive(Debug, Default)]
pub struct MockClock {
    now: AtomicU64,
}

impl MockClock {
    pub fn new(start_ms: u64) -> Self {
        Self {
            now: AtomicU64::new(start_ms),
        }
    }

    /// Advance the mock clock by `ms` milliseconds.
    pub fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::Relaxed);
    }

    /// Set the mock clock to an exact value.
    pub fn set(&self, ms: u64) {
        self.now.store(ms, Ordering::Relaxed);
    }
}

impl Clock for MockClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_starts_at_configured_time() {
        let c = MockClock::new(1_000);
        assert_eq!(c.now_ms(), 1_000);
    }

    #[test]
    fn mock_clock_advances_and_sets() {
        let c = MockClock::new(0);
        c.advance(500);
        assert_eq!(c.now_ms(), 500);
        c.advance(250);
        assert_eq!(c.now_ms(), 750);
        c.set(10);
        assert_eq!(c.now_ms(), 10);
    }

    #[test]
    fn system_clock_is_after_2020_01_01() {
        // 2020-01-01 UTC in ms since UNIX epoch.
        let floor_ms = 1_577_836_800_000;
        let now = SystemClock.now_ms();
        assert!(
            now > floor_ms,
            "system clock returned {now}, which is before 2020-01-01"
        );
    }
}
