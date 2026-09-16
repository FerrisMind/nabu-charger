//! Типизированные ошибки ядра драйвера LN8000.
//!
//! Библиотечный код не паникует: нештатные ситуации возвращаются значениями.

use core::fmt;

/// Категория сбоя шины I²C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusErrorKind {
    /// Ошибка ввода-вывода.
    Io,
    /// Устройство не ответило.
    Timeout,
    /// Устройство отсутствует на шине.
    Disconnected,
    /// Устройство вернуло неожиданный ответ.
    Protocol,
    /// Операция не поддерживается транспортом.
    Unsupported,
}

impl BusErrorKind {
    /// Короткое имя категории для журнала.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Disconnected => "disconnected",
            Self::Protocol => "protocol",
            Self::Unsupported => "unsupported",
        }
    }
}

impl fmt::Display for BusErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Сбой обмена по шине I²C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusError {
    /// Категория.
    pub kind: BusErrorKind,
    /// Платформенный код (NTSTATUS или код протокола), `0` если нет.
    pub code: i32,
    /// Пояснение.
    pub detail: &'static str,
}

impl BusError {
    /// Создаёт ошибку шины.
    #[must_use]
    pub const fn new(kind: BusErrorKind, code: i32, detail: &'static str) -> Self {
        Self { kind, code, detail }
    }

    /// Ошибка ввода-вывода.
    #[must_use]
    pub const fn io(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Io, 0, detail)
    }

    /// Таймаут.
    #[must_use]
    pub const fn timeout(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Timeout, 0, detail)
    }

    /// Устройство отсутствует.
    #[must_use]
    pub const fn disconnected(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Disconnected, 0, detail)
    }

    /// Нарушение протокола.
    #[must_use]
    pub const fn protocol(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Protocol, 0, detail)
    }

    /// Операция не поддерживается.
    #[must_use]
    pub const fn unsupported(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Unsupported, 0, detail)
    }
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "шина: {} (код {}): {}",
            self.kind, self.code, self.detail
        )
    }
}

impl core::error::Error for BusError {}

/// Полная ошибка драйвера LN8000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PumpError {
    /// Сбой шины.
    Bus(BusError),
    /// Сессия не открыта.
    NotOpen,
    /// Устройство ответило не тем идентификатором.
    WrongDeviceId {
        /// Что прочитали из `DEVICE_ID`.
        got: u8,
    },
    /// Устройство не подтвердило нужный режим.
    ModeNotReached {
        /// Ожидаемый режим.
        wanted: u8,
        /// Что показывает `SYS_STS`.
        raw_status: u8,
    },
    /// Сработал отказ (сторожевой таймер, перенапряжение, перегрев…).
    Fault {
        /// Маска отказа.
        mask: u8,
        /// Описание отказа.
        detail: &'static str,
    },
    /// Значение вне допустимого диапазона.
    OutOfRange {
        /// Параметр.
        field: &'static str,
        /// Запрошенное значение.
        requested: u32,
    },
    /// Сторожевой таймер не поддерживали: устройство ушло в shutdown.
    WatchdogExpired,
}

impl PumpError {
    /// Стабильный код ошибки для журнала и метрик.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Bus(_) => "bus",
            Self::NotOpen => "not_open",
            Self::WrongDeviceId { .. } => "wrong_device_id",
            Self::ModeNotReached { .. } => "mode_not_reached",
            Self::Fault { .. } => "fault",
            Self::OutOfRange { .. } => "out_of_range",
            Self::WatchdogExpired => "watchdog_expired",
        }
    }

    /// Можно ли продолжить работу после сброса шины.
    #[must_use]
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Bus(BusError {
                kind: BusErrorKind::Timeout | BusErrorKind::Disconnected | BusErrorKind::Io,
                ..
            }) | Self::ModeNotReached { .. }
        )
    }
}

impl fmt::Display for PumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bus(e) => write!(f, "{e}"),
            Self::NotOpen => f.write_str("сессия не открыта"),
            Self::WrongDeviceId { got } => {
                write!(
                    f,
                    "неожиданный идентификатор устройства: 0x{got:02X} (ожидался 0x42)"
                )
            }
            Self::ModeNotReached { wanted, raw_status } => {
                write!(f, "режим {wanted} не достигнут, SYS_STS=0x{raw_status:02X}")
            }
            Self::Fault { mask, detail } => {
                write!(f, "отказ (маска 0x{mask:02X}): {detail}")
            }
            Self::OutOfRange { field, requested } => {
                write!(f, "значение вне диапазона: {field}={requested}")
            }
            Self::WatchdogExpired => f.write_str("истёк сторожевой таймер"),
        }
    }
}

impl core::error::Error for PumpError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Bus(e) => Some(e),
            _ => None,
        }
    }
}

impl From<BusError> for PumpError {
    fn from(value: BusError) -> Self {
        Self::Bus(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(PumpError::NotOpen.code(), "not_open");
        assert_eq!(PumpError::Bus(BusError::timeout("x")).code(), "bus");
        assert_eq!(PumpError::WatchdogExpired.code(), "watchdog_expired");
    }

    #[test]
    fn recoverability_classifies_errors() {
        assert!(PumpError::Bus(BusError::timeout("x")).is_recoverable());
        assert!(
            PumpError::ModeNotReached {
                wanted: 3,
                raw_status: 1
            }
            .is_recoverable()
        );
        assert!(!PumpError::NotOpen.is_recoverable());
        assert!(!PumpError::WatchdogExpired.is_recoverable());
    }

    #[test]
    fn display_mentions_details() {
        let error = PumpError::WrongDeviceId { got: 0x00 };
        assert!(error.to_string().contains("0x00"));
        let bus = BusError::disconnected("нет ответа");
        assert!(bus.to_string().contains("disconnected"));
    }

    #[test]
    fn errors_implement_core_error() {
        fn assert_error<T: core::error::Error>() {}
        assert_error::<PumpError>();
        assert_error::<BusError>();
    }
}
