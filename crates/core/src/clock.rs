//! Time source.
//!
//! The core never sleeps and never touches the system clock: it takes time from
//! the outside. This makes detection timeouts deterministic and fully testable.

/// Monotonic clock in milliseconds.
pub trait Clock {
    /// Current monotonic time in milliseconds.
    fn now_ms(&self) -> u64;
}

/// Manually controlled clock for tests and demonstrations.
///
/// Time moves only explicitly, through [`ManualClock::advance_ms`].
#[derive(Debug, Default)]
pub struct ManualClock {
    now_ms: core::cell::Cell<u64>,
}

impl ManualClock {
    /// Creates a clock that starts counting from zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            now_ms: core::cell::Cell::new(0),
        }
    }

    /// Creates a clock with a given initial value.
    #[must_use]
    pub const fn starting_at(now_ms: u64) -> Self {
        Self {
            now_ms: core::cell::Cell::new(now_ms),
        }
    }

    /// Advances time forward.
    pub fn advance_ms(&self, delta_ms: u64) {
        self.now_ms.set(self.now_ms.get().saturating_add(delta_ms));
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.get()
    }
}
