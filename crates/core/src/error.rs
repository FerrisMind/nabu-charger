//! Типизированные ошибки ядра драйвера.
//!
//! В библиотечном коде нет `unwrap`, `expect` и `panic`: любая нештатная
//! ситуация возвращается как значение. Строки в ошибках статические, поэтому
//! крейт остаётся пригодным для `no_std`.

use core::fmt;

/// Категория сбоя транспорта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// Ошибка ввода-вывода на стороне ОС.
    Io,
    /// Устройство не ответило за отведённое время.
    Timeout,
    /// Связь с устройством потеряна (устройство исчезло из системы).
    Disconnected,
    /// Устройство вернуло неверный или неразборчивый ответ.
    Protocol,
    /// Операция не поддерживается данным транспортом.
    Unsupported,
}

impl TransportErrorKind {
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

impl fmt::Display for TransportErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Сбой на уровне транспорта: не удалось прочитать или записать регистр.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportError {
    /// Категория сбоя.
    pub kind: TransportErrorKind,
    /// Платформенный код (код NTSTATUS, `errno` или код из протокола), `0` если нет.
    pub code: i32,
    /// Человекочитаемое пояснение.
    pub detail: &'static str,
}

impl TransportError {
    /// Создаёт ошибку транспорта.
    #[must_use]
    pub const fn new(kind: TransportErrorKind, code: i32, detail: &'static str) -> Self {
        Self { kind, code, detail }
    }

    /// Сбой ввода-вывода.
    #[must_use]
    pub const fn io(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Io, 0, detail)
    }

    /// Таймаут операции.
    #[must_use]
    pub const fn timeout(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Timeout, 0, detail)
    }

    /// Потеря связи с устройством.
    #[must_use]
    pub const fn disconnected(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Disconnected, 0, detail)
    }

    /// Нарушение протокола обмена.
    #[must_use]
    pub const fn protocol(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Protocol, 0, detail)
    }

    /// Операция не поддерживается.
    #[must_use]
    pub const fn unsupported(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Unsupported, 0, detail)
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "транспорт: {} (код {}): {}",
            self.kind, self.code, self.detail
        )
    }
}

impl core::error::Error for TransportError {}

/// Полная ошибка драйвера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChargerError {
    /// Не удалось выполнить операцию с регистрами.
    Transport(TransportError),
    /// Операция требует открытой сессии.
    NotOpen,
    /// Сессия уже открыта.
    AlreadyOpen,
    /// APSD ещё не завершился: результат пока недостоверен.
    DetectionNotComplete,
    /// APSD не завершился за отведённое время.
    DetectionTimeout {
        /// Сколько миллисекунд ждали.
        waited_ms: u64,
    },
    /// `APSD_RESULT_STATUS` содержит неизвестный образец.
    UnknownAdapterPattern {
        /// Прочитанное значение без служебного бита.
        raw: u8,
    },
    /// Аппаратура сообщила таймаут проверки HVDCP: адаптер нестабилен.
    AdapterCheckTimeout {
        /// Сырое значение `APSD_STATUS`.
        raw_status: u8,
    },
    /// Запрошенный ток вне допустимого диапазона.
    CurrentOutOfRange {
        /// Запрошенный ток в микроамперax.
        requested_ua: u32,
        /// Верхняя граница в микроамперax.
        max_ua: u32,
    },
    /// Прочитанное значение не совпало с записанным.
    VerifyFailed {
        /// Адрес регистра.
        addr: u16,
        /// Что записали.
        wrote: u8,
        /// Что прочитали.
        read: u8,
    },
    /// Устройство сообщило о неисправности.
    DeviceFault {
        /// Сырое значение регистра состояния.
        status: u8,
    },
}

impl ChargerError {
    /// Короткое стабильное имя ошибки для журнала и метрик.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::NotOpen => "not_open",
            Self::AlreadyOpen => "already_open",
            Self::DetectionNotComplete => "detection_not_complete",
            Self::DetectionTimeout { .. } => "detection_timeout",
            Self::UnknownAdapterPattern { .. } => "unknown_adapter_pattern",
            Self::AdapterCheckTimeout { .. } => "adapter_check_timeout",
            Self::CurrentOutOfRange { .. } => "current_out_of_range",
            Self::VerifyFailed { .. } => "verify_failed",
            Self::DeviceFault { .. } => "device_fault",
        }
    }

    /// Позволяет ли ошибка продолжить работу после сброса транспорта.
    #[must_use]
    pub const fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Transport(TransportError {
                kind: TransportErrorKind::Timeout | TransportErrorKind::Disconnected,
                ..
            }) | Self::DetectionNotComplete
                | Self::DetectionTimeout { .. }
                | Self::AdapterCheckTimeout { .. }
        )
    }
}

impl fmt::Display for ChargerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::NotOpen => f.write_str("сессия не открыта"),
            Self::AlreadyOpen => f.write_str("сессия уже открыта"),
            Self::DetectionNotComplete => f.write_str("детекция APSD ещё не завершена"),
            Self::DetectionTimeout { waited_ms } => {
                write!(f, "детекция APSD не завершилась за {waited_ms} мс")
            }
            Self::UnknownAdapterPattern { raw } => {
                write!(f, "неизвестный образец APSD: 0x{raw:02X}")
            }
            Self::AdapterCheckTimeout { raw_status } => write!(
                f,
                "аппаратура сообщила таймаут проверки HVDCP (APSD_STATUS=0x{raw_status:02X})"
            ),
            Self::CurrentOutOfRange {
                requested_ua,
                max_ua,
            } => write!(
                f,
                "ток {requested_ua} мкА выше допустимого максимума {max_ua} мкА"
            ),
            Self::VerifyFailed { addr, wrote, read } => write!(
                f,
                "проверка записи не прошла: 0x{addr:04X} записали 0x{wrote:02X}, прочитали 0x{read:02X}"
            ),
            Self::DeviceFault { status } => {
                write!(f, "устройство сообщило о неисправности: 0x{status:02X}")
            }
        }
    }
}

impl core::error::Error for ChargerError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl From<TransportError> for ChargerError {
    fn from(value: TransportError) -> Self {
        Self::Transport(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ChargerError::NotOpen.code(), "not_open");
        assert_eq!(
            ChargerError::Transport(TransportError::timeout("x")).code(),
            "transport"
        );
        assert_eq!(
            ChargerError::DetectionTimeout { waited_ms: 10 }.code(),
            "detection_timeout"
        );
    }

    #[test]
    fn recoverability_classifies_errors() {
        assert!(ChargerError::Transport(TransportError::timeout("x")).is_recoverable());
        assert!(ChargerError::DetectionNotComplete.is_recoverable());
        assert!(!ChargerError::NotOpen.is_recoverable());
        assert!(!ChargerError::UnknownAdapterPattern { raw: 1 }.is_recoverable());
    }

    #[test]
    fn display_mentions_details() {
        let error = ChargerError::VerifyFailed {
            addr: 0x1370,
            wrote: 0x1D,
            read: 0x00,
        };
        let text = error.to_string();
        assert!(text.contains("0x1370"));
        assert!(text.contains("0x1D"));
        let transport = TransportError::disconnected("канал потерян");
        assert!(transport.to_string().contains("disconnected"));
    }

    #[test]
    fn errors_implement_std_error() {
        fn assert_error<T: core::error::Error>() {}
        assert_error::<ChargerError>();
        assert_error::<TransportError>();
    }
}
