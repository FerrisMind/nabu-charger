//! Driver contract with user mode: IOCTL codes and exchange structures.
//!
//! All requests are `METHOD_BUFFERED`, `FILE_ANY_ACCESS`, device type
//! `FILE_DEVICE_UNKNOWN` (0x22). The codes are built with the standard macro
//! `CTL_CODE(Type, Function, Method, Access) = (Type << 16) | (Access << 14) | (Function << 2) | Method`.

/// Device type declared by the driver.
pub const FILE_DEVICE_NABU_CHARGER: u32 = 0x22;

/// Builds a control code per the `CTL_CODE` rules.
///
/// * `function` - function number (0x800..0xFFF for vendor codes);
/// * `method` - 0 = `METHOD_BUFFERED`;
/// * `access` - 0 = `FILE_ANY_ACCESS`.
#[must_use]
pub const fn ctl_code(function: u32, method: u32, access: u32) -> u32 {
    (FILE_DEVICE_NABU_CHARGER << 16) | (access << 14) | (function << 2) | method
}

/// Get the driver state and the last applied plan.
pub const IOCTL_NABU_GET_STATUS: u32 = ctl_code(0x800, 0, 0);
/// Start adapter detection (non-blocking: the result appears in the status).
pub const IOCTL_NABU_DETECT_START: u32 = ctl_code(0x801, 0, 0);
/// Apply the current policy for the recognized adapter.
pub const IOCTL_NABU_APPLY_POLICY: u32 = ctl_code(0x802, 0, 0);
/// Force the input current limit in microamperes.
pub const IOCTL_NABU_SET_ICL: u32 = ctl_code(0x803, 0, 0);
/// Read a charger peripheral register (diagnostics).
pub const IOCTL_NABU_READ_REG: u32 = ctl_code(0x804, 0, 0);
/// Write a charger peripheral register (diagnostics).
pub const IOCTL_NABU_WRITE_REG: u32 = ctl_code(0x805, 0, 0);
/// Get a snapshot of the operation journal.
pub const IOCTL_NABU_GET_JOURNAL: u32 = ctl_code(0x806, 0, 0);

/// Structure identifier so the client cannot mix up versions.
pub const NABU_STATUS_MAGIC: u32 = 0x4E41_4255; // "NABU"

/// Version of the exchange contract.
pub const NABU_STATUS_VERSION: u16 = 1;

/// Driver capabilities: a single extension stream, as is customary in Windows.
pub const NABU_CAPABILITIES: u32 = 1;

/// Driver session state (matches `charger_core::State`).
///
/// During bring-up all variants are used: `Idle`/`Detecting`/`Ready` after the
/// detection timer is wired up (see `docs/HANDOVER.md`).
#[allow(dead_code)]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NabuState {
    /// The session is not open.
    Closed = 0,
    /// Communication is up, detection has not been started.
    Idle = 1,
    /// Waiting for APSD.
    Detecting = 2,
    /// The type is determined, the policy has been applied.
    Ready = 3,
    /// The session is faulted.
    Faulted = 4,
}

impl NabuState {
    /// Converts the core state into the contract representation.
    #[allow(dead_code)] // Wired up together with the detection timer.
    #[must_use]
    pub const fn from_core(state: charger_core::State) -> Self {
        match state {
            charger_core::State::Closed => Self::Closed,
            charger_core::State::Idle => Self::Idle,
            charger_core::State::Detecting => Self::Detecting,
            charger_core::State::Ready => Self::Ready,
            charger_core::State::Faulted => Self::Faulted,
        }
    }
}

/// Response to [`IOCTL_NABU_GET_STATUS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuStatus {
    /// Magic value of [`NABU_STATUS_MAGIC`].
    pub magic: u32,
    /// Contract version of [`NABU_STATUS_VERSION`].
    pub version: u16,
    /// Capability bit mask.
    pub capabilities: u32,
    /// Session state.
    pub state: u8,
    /// Adapter type code (`AdapterType` as a number; 255 means unknown).
    pub adapter_code: u8,
    /// Whether the charge pump is eligible.
    pub pump_eligible: u8,
    /// Reserved for alignment.
    pub reserved: u8,
    /// Actual input current limit in microamperes.
    pub icl_ua: u32,
    /// Limit code written to the register.
    pub icl_raw: u8,
    /// Reserved.
    pub reserved2: [u8; 3],
    /// How many register reads have been performed.
    pub reads: u64,
    /// How many writes have been performed.
    pub writes: u64,
    /// How many retries after failures.
    pub retries: u64,
    /// How many channel resets.
    pub resets: u64,
    /// How many errors have been recorded.
    pub errors: u64,
}

/// Request for [`IOCTL_NABU_READ_REG`] and [`IOCTL_NABU_WRITE_REG`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuRegRequest {
    /// Register address.
    pub addr: u16,
    /// Value: input for a write, output for a read.
    pub value: u8,
    /// Reserved.
    pub reserved: u8,
    /// Transport error code if the operation failed.
    pub error_code: i32,
}

/// Request for [`IOCTL_NABU_SET_ICL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuIclRequest {
    /// Requested input current limit in microamperes.
    pub icl_ua: u32,
    /// Actually applied limit (after quantization to the grid).
    pub applied_ua: u32,
    /// Register code that was written.
    pub icl_raw: u8,
    /// Error code: 0 means success.
    pub error_code: i32,
}

/// One journal record in the buffer of [`IOCTL_NABU_GET_JOURNAL`].
#[allow(dead_code)] // Filled in when journal delivery to the client is wired up.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuJournalEntry {
    /// Sequence number.
    pub seq: u64,
    /// Timestamp in milliseconds of the kernel monotonic clock.
    pub ts_ms: u64,
    /// Request identifier.
    pub request_id: u64,
    /// Level (`trace`..`error`).
    pub level: u8,
    /// Event kind.
    pub kind: u8,
    /// Register address, if applicable.
    pub addr: u16,
    /// Value, if applicable.
    pub value: u8,
    /// Reserved.
    pub reserved: [u8; 3],
}

/// Request for [`IOCTL_NABU_GET_JOURNAL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuJournalRequest {
    /// How many records to return (not more than the client buffer size).
    pub count: u32,
    /// How many records are actually available.
    pub available: u32,
}
