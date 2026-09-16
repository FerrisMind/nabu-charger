//! Источник времени.
//!
//! Ядро не спит и не обращается к системным часам: оно принимает время извне.
//! Это делает таймауты детекции детерминированными и полностью проверяемыми.

/// Монотонные часы в миллисекундах.
pub trait Clock {
    /// Текущее монотонное время в миллисекундах.
    fn now_ms(&self) -> u64;
}

/// Часы с ручным управлением для тестов и демонстраций.
///
/// Время двигается только явно, через [`ManualClock::advance_ms`].
#[derive(Debug, Default)]
pub struct ManualClock {
    now_ms: core::cell::Cell<u64>,
}

impl ManualClock {
    /// Создаёт часы, начинающие отсчёт с нуля.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            now_ms: core::cell::Cell::new(0),
        }
    }

    /// Создаёт часы с заданным начальным значением.
    #[must_use]
    pub const fn starting_at(now_ms: u64) -> Self {
        Self {
            now_ms: core::cell::Cell::new(now_ms),
        }
    }

    /// Продвигает время вперёд.
    pub fn advance_ms(&self, delta_ms: u64) {
        self.now_ms.set(self.now_ms.get().saturating_add(delta_ms));
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.get()
    }
}
