//! Monotonic process clock.

use charger_core::Clock;
use std::time::Instant;

/// Clock based on [`Instant`]: monotonic, no system time jumps.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Starts counting from the current moment.
    #[must_use]
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// How much time has passed since the start.
    #[must_use]
    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // Process time will not exceed u64::MAX milliseconds in any foreseeable period.
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Clock that can be moved by hand: for deterministic demos.
#[derive(Debug, Default)]
pub struct AdjustableClock {
    offset_ms: std::sync::atomic::AtomicU64,
}

impl AdjustableClock {
    /// Creates a clock with zero offset.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            offset_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Adds an offset.
    pub fn advance_ms(&self, delta_ms: u64) {
        let _ = self
            .offset_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Clock for AdjustableClock {
    fn now_ms(&self) -> u64 {
        self.offset_ms.load(std::sync::atomic::Ordering::Relaxed)
    }
}
