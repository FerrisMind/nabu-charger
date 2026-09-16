//! Монотонные часы процесса.

use charger_core::Clock;
use std::time::Instant;

/// Часы на основе [`Instant`]: монотонные, без перескоков системного времени.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Запускает отсчёт от текущего момента.
    #[must_use]
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// Сколько времени прошло с момента запуска.
    #[must_use]
    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // Время процесса не превысит u64::MAX миллисекунд за обозримый срок.
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Часы, которые можно сдвинуть руками: для детерминированных демонстраций.
#[derive(Debug, Default)]
pub struct AdjustableClock {
    offset_ms: std::sync::atomic::AtomicU64,
}

impl AdjustableClock {
    /// Создаёт часы с нулевым смещением.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            offset_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Добавляет смещение.
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
