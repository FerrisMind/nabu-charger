//! Typed errors of the LN8000 driver core.
//!
//! Library code does not panic: abnormal situations are returned by value.

use core::fmt;

/// Category of an I²C bus failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusErrorKind {
    /// Input/output error.
    Io,
    /// The device did not answer.
    Timeout,
    /// The device is absent from the bus.
    Disconnected,
    /// The device returned an unexpected response.
    Protocol,
    /// The operation is not supported by the transport.
    Unsupported,
}

impl BusErrorKind {
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

impl fmt::Display for BusErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Failure of an I²C bus transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusError {
    /// Category.
    pub kind: BusErrorKind,
    /// Platform code (NTSTATUS or protocol code), `0` if none.
    pub code: i32,
    /// Explanation.
    pub detail: &'static str,
}

impl BusError {
    /// Creates a bus error.
    #[must_use]
    pub const fn new(kind: BusErrorKind, code: i32, detail: &'static str) -> Self {
        Self { kind, code, detail }
    }

    /// Input/output error.
    #[must_use]
    pub const fn io(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Io, 0, detail)
    }

    /// Timeout.
    #[must_use]
    pub const fn timeout(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Timeout, 0, detail)
    }

    /// The device is absent.
    #[must_use]
    pub const fn disconnected(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Disconnected, 0, detail)
    }

    /// Protocol violation.
    #[must_use]
    pub const fn protocol(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Protocol, 0, detail)
    }

    /// The operation is not supported.
    #[must_use]
    pub const fn unsupported(detail: &'static str) -> Self {
        Self::new(BusErrorKind::Unsupported, 0, detail)
    }
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bus: {} (code {}): {}",
            self.kind, self.code, self.detail
        )
    }
}

impl core::error::Error for BusError {}

/// Full LN8000 driver error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PumpError {
    /// Bus failure.
    Bus(BusError),
    /// Session is not open.
    NotOpen,
    /// The device answered with the wrong identifier.
    WrongDeviceId {
        /// What was read from `DEVICE_ID`.
        got: u8,
    },
    /// The device did not confirm the requested mode.
    ModeNotReached {
        /// Expected mode.
        wanted: u8,
        /// What `SYS_STS` shows.
        raw_status: u8,
    },
    /// 1:1 bypass forbidden: the input is outside the bypass window (4.2-8 V needed).
    ///
    /// `EN_1TO1` feeds the input straight to the battery, so with a raised Vin
    /// (QC/PD, 9-12 V) the mode is not entered by any path.
    BypassNeedsFiveVoltVin {
        /// Measured Vin, µV (may be negative on ADC failure).
        vin_uv: i32,
    },
    /// A fault fired (watchdog timer, overvoltage, overtemperature...).
    Fault {
        /// Fault mask.
        mask: u8,
        /// Fault description.
        detail: &'static str,
    },
    /// Value outside the allowed range.
    OutOfRange {
        /// Parameter.
        field: &'static str,
        /// Requested value.
        requested: u32,
    },
    /// The watchdog timer was not serviced: the device went to shutdown.
    WatchdogExpired,
}

impl PumpError {
    /// Stable error code for the journal and metrics.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Bus(_) => "bus",
            Self::NotOpen => "not_open",
            Self::WrongDeviceId { .. } => "wrong_device_id",
            Self::ModeNotReached { .. } => "mode_not_reached",
            Self::BypassNeedsFiveVoltVin { .. } => "bypass_needs_five_volt_vin",
            Self::Fault { .. } => "fault",
            Self::OutOfRange { .. } => "out_of_range",
            Self::WatchdogExpired => "watchdog_expired",
        }
    }

    /// Whether work can continue after a bus reset.
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
            Self::NotOpen => f.write_str("session is not open"),
            Self::WrongDeviceId { got } => {
                write!(
                    f,
                    "unexpected device identifier: 0x{got:02X} (expected 0x42)"
                )
            }
            Self::ModeNotReached { wanted, raw_status } => {
                write!(f, "mode {wanted} not reached, SYS_STS=0x{raw_status:02X}")
            }
            Self::BypassNeedsFiveVoltVin { vin_uv } => {
                write!(
                    f,
                    "1:1 bypass forbidden at Vin {vin_uv} µV (4.2-8 V needed)"
                )
            }
            Self::Fault { mask, detail } => {
                write!(f, "fault (mask 0x{mask:02X}): {detail}")
            }
            Self::OutOfRange { field, requested } => {
                write!(f, "value out of range: {field}={requested}")
            }
            Self::WatchdogExpired => f.write_str("watchdog timer expired"),
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
        let bus = BusError::disconnected("no response");
        assert!(bus.to_string().contains("disconnected"));
    }

    #[test]
    fn errors_implement_core_error() {
        fn assert_error<T: core::error::Error>() {}
        assert_error::<PumpError>();
        assert_error::<BusError>();
    }
}
