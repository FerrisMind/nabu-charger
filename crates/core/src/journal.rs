//! Structured operation journal.
//!
//! Every access to the device and every driver decision goes into the journal:
//! with a sequence number, a timestamp, a request id and the outcome.
//! The host layer turns these records into JSON Lines and into `tracing` events.
//!
//! All text fields are static strings, so records are copyable, need no allocations
//! and work in `no_std`.

/// Record severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Details needed only when debugging.
    Trace,
    /// Diagnostic information.
    Debug,
    /// Normal operation.
    Info,
    /// Abnormal situation, operation continues.
    Warn,
    /// Operation not completed.
    Error,
}

impl Level {
    /// Level name for serialization.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl core::fmt::Display for Level {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What exactly happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// Session open: link check with the peripheral.
    Open {
        /// Transport name.
        transport: &'static str,
        /// Whether the open succeeded.
        ok: bool,
    },
    /// Session close.
    Close {
        /// Whether the safe current limit was restored.
        ok: bool,
    },
    /// Register read.
    Read {
        /// Register address.
        addr: u16,
        /// Value read.
        value: u8,
        /// Operation duration in microseconds.
        elapsed_us: u64,
    },
    /// Register write.
    Write {
        /// Register address.
        addr: u16,
        /// Value written.
        value: u8,
        /// Operation duration in microseconds.
        elapsed_us: u64,
    },
    /// Adapter detection result.
    Detect {
        /// Identified type.
        adapter: &'static str,
        /// Raw value of `APSD_STATUS`.
        raw_status: u8,
        /// Raw value of `APSD_RESULT_STATUS`.
        raw_result: u8,
        /// How many milliseconds detection took.
        waited_ms: u64,
    },
    /// Applied current policy.
    Policy {
        /// Adapter type.
        adapter: &'static str,
        /// Target current limit in microamperes.
        icl_ua: u32,
        /// Code written to the register.
        icl_raw: u8,
        /// QC2 voltage, if requested.
        qc2_voltage: Option<&'static str>,
        /// Whether the charge pump is eligible.
        pump_eligible: bool,
    },
    /// Retry.
    Retry {
        /// What is being retried.
        op: &'static str,
        /// Attempt number.
        attempt: u8,
        /// Reason.
        reason: &'static str,
    },
    /// Link reset.
    Reset {
        /// Whether the link was recovered.
        ok: bool,
    },
    /// Driver state change.
    StateChange {
        /// Previous state.
        from: &'static str,
        /// New state.
        to: &'static str,
    },
    /// Error.
    Error {
        /// Operation.
        op: &'static str,
        /// Error code.
        error: &'static str,
    },
}

impl EventKind {
    /// Stable event type name for filtering and metrics.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Close { .. } => "close",
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Detect { .. } => "detect",
            Self::Policy { .. } => "policy",
            Self::Retry { .. } => "retry",
            Self::Reset { .. } => "reset",
            Self::StateChange { .. } => "state",
            Self::Error { .. } => "error",
        }
    }
}

/// One journal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Record sequence number, starting at 1.
    pub seq: u64,
    /// Timestamp in milliseconds of the monotonic clock.
    pub ts_ms: u64,
    /// Request id the record belongs to.
    pub request_id: u64,
    /// Severity level.
    pub level: Level,
    /// Content.
    pub kind: EventKind,
}

/// Sink for journal records.
///
/// The implementation must not panic and must be ready to be called from `Drop`.
pub trait Journal {
    /// Accepts one record.
    fn event(&self, event: &Event);
}

/// Null journal: does nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullJournal;

impl Journal for NullJournal {
    fn event(&self, _event: &Event) {}
}
