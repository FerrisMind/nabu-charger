//! Typed core driver errors.
//!
//! Library code contains no `unwrap`, `expect` or `panic`: every abnormal situation
//! is returned as a value. The strings in errors are static, so the crate stays
//! usable in `no_std`.

use core::fmt;

/// Transport failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// OS-side input/output error.
    Io,
    /// The device did not respond in the allotted time.
    Timeout,
    /// Communication with the device was lost (the device left the system).
    Disconnected,
    /// The device returned an invalid or unreadable response.
    Protocol,
    /// The operation is not supported by this transport.
    Unsupported,
}

impl TransportErrorKind {
    /// Short category name for the journal.
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

/// Transport-level failure: a register could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportError {
    /// Failure category.
    pub kind: TransportErrorKind,
    /// Platform code (NTSTATUS code, `errno` or protocol code), `0` if none.
    pub code: i32,
    /// Human-readable explanation.
    pub detail: &'static str,
}

impl TransportError {
    /// Creates a transport error.
    #[must_use]
    pub const fn new(kind: TransportErrorKind, code: i32, detail: &'static str) -> Self {
        Self { kind, code, detail }
    }

    /// Input/output failure.
    #[must_use]
    pub const fn io(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Io, 0, detail)
    }

    /// Operation timeout.
    #[must_use]
    pub const fn timeout(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Timeout, 0, detail)
    }

    /// Loss of communication with the device.
    #[must_use]
    pub const fn disconnected(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Disconnected, 0, detail)
    }

    /// Protocol violation.
    #[must_use]
    pub const fn protocol(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Protocol, 0, detail)
    }

    /// Operation not supported.
    #[must_use]
    pub const fn unsupported(detail: &'static str) -> Self {
        Self::new(TransportErrorKind::Unsupported, 0, detail)
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "transport: {} (code {}): {}",
            self.kind, self.code, self.detail
        )
    }
}

impl core::error::Error for TransportError {}

/// Complete driver error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChargerError {
    /// Register operation failed.
    Transport(TransportError),
    /// The operation requires an open session.
    NotOpen,
    /// The session is already open.
    AlreadyOpen,
    /// APSD has not completed yet: the result is not yet reliable.
    DetectionNotComplete,
    /// APSD did not complete in the allotted time.
    DetectionTimeout {
        /// How many milliseconds were waited.
        waited_ms: u64,
    },
    /// `APSD_RESULT_STATUS` holds an unknown pattern.
    UnknownAdapterPattern {
        /// Value read without the service bit.
        raw: u8,
    },
    /// The hardware reported an HVDCP check timeout: the adapter is unstable.
    AdapterCheckTimeout {
        /// Raw value of `APSD_STATUS`.
        raw_status: u8,
    },
    /// Requested current is outside the allowed range.
    CurrentOutOfRange {
        /// Requested current in microamperes.
        requested_ua: u32,
        /// Upper bound in microamperes.
        max_ua: u32,
    },
    /// The value read did not match the value written.
    VerifyFailed {
        /// Register address.
        addr: u16,
        /// What was written.
        wrote: u8,
        /// What was read.
        read: u8,
    },
    /// The device reported a fault.
    DeviceFault {
        /// Raw value of the status register.
        status: u8,
    },
}

impl ChargerError {
    /// Short stable error name for the journal and metrics.
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

    /// Whether the error still allows work to continue after a transport reset.
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
            Self::NotOpen => f.write_str("session is not open"),
            Self::AlreadyOpen => f.write_str("session is already open"),
            Self::DetectionNotComplete => f.write_str("APSD detection has not completed yet"),
            Self::DetectionTimeout { waited_ms } => {
                write!(f, "APSD detection did not complete within {waited_ms} ms")
            }
            Self::UnknownAdapterPattern { raw } => {
                write!(f, "unknown APSD pattern: 0x{raw:02X}")
            }
            Self::AdapterCheckTimeout { raw_status } => write!(
                f,
                "hardware reported an HVDCP check timeout (APSD_STATUS=0x{raw_status:02X})"
            ),
            Self::CurrentOutOfRange {
                requested_ua,
                max_ua,
            } => write!(
                f,
                "current {requested_ua} µA is above the allowed maximum {max_ua} µA"
            ),
            Self::VerifyFailed { addr, wrote, read } => write!(
                f,
                "write verification failed: 0x{addr:04X} wrote 0x{wrote:02X}, read 0x{read:02X}"
            ),
            Self::DeviceFault { status } => {
                write!(f, "device reported a fault: 0x{status:02X}")
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
        let transport = TransportError::disconnected("link lost");
        assert!(transport.to_string().contains("disconnected"));
    }

    #[test]
    fn errors_implement_std_error() {
        fn assert_error<T: core::error::Error>() {}
        assert_error::<ChargerError>();
        assert_error::<TransportError>();
    }
}
