//! Contract between the LN8000 driver and user mode.
//!
//! All requests are `METHOD_BUFFERED`, `FILE_ANY_ACCESS`, device type
//! `FILE_DEVICE_UNKNOWN` (0x22). The codes are built with the standard macro
//! `CTL_CODE(Type, Function, Method, Access) = (Type << 16) | (Access << 14) | (Function << 2) | Method`.

/// LN8000 driver device type.
pub const FILE_DEVICE_LN8000: u32 = 0x22;

/// Builds a control code by the `CTL_CODE` rules.
#[must_use]
pub const fn ctl_code(function: u32, method: u32, access: u32) -> u32 {
    (FILE_DEVICE_LN8000 << 16) | (access << 14) | (function << 2) | method
}

/// Driver status: mode, failures, last sample, session counters.
pub const IOCTL_LN8000_GET_STATUS: u32 = ctl_code(0x810, 0, 0);
/// Read an LN8000 register directly (diagnostics).
pub const IOCTL_LN8000_READ_REG: u32 = ctl_code(0x811, 0, 0);
/// Write an LN8000 register directly (diagnostics).
pub const IOCTL_LN8000_WRITE_REG: u32 = ctl_code(0x812, 0, 0);
/// Set the limits: input current and charge voltage.
pub const IOCTL_LN8000_SET_LIMITS: u32 = ctl_code(0x813, 0, 0);
/// Switch the mode: standby / bypass / switching.
pub const IOCTL_LN8000_SET_MODE: u32 = ctl_code(0x814, 0, 0);
/// Get charge session info (current and last completed).
pub const IOCTL_LN8000_GET_SESSIONS: u32 = ctl_code(0x815, 0, 0);
/// Retrieve the latest telemetry samples.
pub const IOCTL_LN8000_GET_SAMPLES: u32 = ctl_code(0x816, 0, 0);

/// Explicit charge start/stop.
///
/// Mirrors the reference `psy_chg_set_charging_enable` sequence: disable
/// reverse-current protection, request the op mode, read the mode back and
/// report what the chip actually answered. Unlike the automatic path this
/// never fails silently: the caller sees the raw `SYS_STS`.
pub const IOCTL_LN8000_SET_CHARGE: u32 = ctl_code(0x817, 0, 0);

/// `error_code` for refusing to enable 1:1 outside the bypass window.
///
/// A separate code (not `-4`): this is not a chip failure but a policy refusal -
/// 1:1 feeds the input straight to the battery, so at `Vin >= 8 V` (or below
/// 4.2 V) the mode is not enabled either through `SET_MODE` or automatically.
pub const ERR_BYPASS_VIN_OUT_OF_WINDOW: i32 = -20;

/// Run HVDCP / QC negotiate (SUPERUSER preferred; Usbin RH secondary).
///
/// On stock ACPI (`UsbinConn=0`) opens `\Device\Spmi\SUPERUSER`, grants peri
/// `0x13`, enables `0x1362`, reruns APSD, then QC2 FORCE_9V or QC3 pulses.
/// Returns `error_code = -10` only when **both** SUPERUSER and Usbin RH fail.
pub const IOCTL_LN8000_RUN_HVDCP: u32 = ctl_code(0x818, 0, 0);

/// Status structure identifier.
pub const LN8000_STATUS_MAGIC: u32 = 0x4C4E_3830; // "LN80"

/// Contract version.
pub const LN8000_STATUS_VERSION: u16 = 1;

/// Driver status for [`IOCTL_LN8000_GET_STATUS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000Status {
    /// Magic [`LN8000_STATUS_MAGIC`].
    pub magic: u32,
    /// Contract version.
    pub version: u16,
    /// Operating mode (0 - unknown, 1 - standby, 2 - bypass, 3 - switching).
    pub op_mode: u8,
    /// Driver session state code (0 - closed, 1 - identified, 2 - configured,
    /// 3 - switching, 4 - failure).
    pub state: u8,
    /// Raw `SYS_STS` value.
    pub sys_sts: u8,
    /// Raw `FAULT1_STS` value.
    pub fault1_sts: u8,
    /// Raw `FAULT2_STS` value.
    pub fault2_sts: u8,
    /// Raw `SAFETY_STS` value.
    pub safety_sts: u8,
    /// Whether there is a critical failure.
    pub critical_fault: u8,
    /// Reserved.
    pub reserved: [u8; 2],
    /// Last measured input current, µA.
    pub iin_ua: u32,
    /// Last measured battery voltage, µV.
    pub vbat_uv: u32,
    /// Last measured input voltage, µV.
    pub vbus_uv: u32,
    /// Last die temperature, tenths of °C.
    pub die_temp_dc: i32,
    /// Total charge sessions.
    pub sessions: u64,
    /// Total telemetry samples.
    pub samples: u64,
    /// Number of writes to the device performed.
    pub writes: u32,
    /// Number of reads performed.
    pub reads: u32,
    /// Last error code (0 - none).
    pub last_error: i32,
}

/// Request for [`IOCTL_LN8000_READ_REG`] and [`IOCTL_LN8000_WRITE_REG`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000RegRequest {
    /// Register address.
    pub addr: u8,
    /// Value: input for a write, output for a read.
    pub value: u8,
    /// Reserved.
    pub reserved: [u8; 2],
    /// Error code.
    pub error_code: i32,
}

/// Request for [`IOCTL_LN8000_SET_LIMITS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000LimitsRequest {
    /// Input current limit, µA (0 - do not change).
    pub iin_ua: u32,
    /// Charge voltage target, µV (0 - do not change).
    pub vbat_uv: u32,
    /// Actually applied current, µA.
    pub applied_iin_ua: u32,
    /// Error code.
    pub error_code: i32,
}

/// Request for [`IOCTL_LN8000_SET_MODE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000ModeRequest {
    /// Requested mode (1 - standby, 2 - bypass, 3 - switching).
    ///
    /// `2` is enabled only inside the bypass window (`Vin` 4.2...8 V): at a
    /// raised voltage `error_code = -20` is returned
    /// ([`ERR_BYPASS_VIN_OUT_OF_WINDOW`]), because 1:1 feeds the input straight
    /// to the battery.
    pub mode: u8,
    /// Actual mode after the switch.
    pub applied_mode: u8,
    /// Reserved.
    pub reserved: [u8; 2],
    /// Error code.
    pub error_code: i32,
}

/// Explicit charge start/stop request for [`IOCTL_LN8000_SET_CHARGE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000ChargeRequest {
    /// 1 = start charging (switching mode), 0 = stop charging (standby).
    pub on: u8,
    /// Op mode actually reported by the chip after the attempt.
    pub applied_mode: u8,
    /// Raw `SYS_STS` read back from the chip.
    pub sys_sts: u8,
    /// Reserved.
    pub reserved: u8,
    /// Error code (0 = ok, negative = pump error).
    pub error_code: i32,
}

/// Session info for [`IOCTL_LN8000_GET_SESSIONS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000Sessions {
    /// Total sessions started.
    pub total: u64,
    /// Current session duration, ms (0 - no input power).
    pub current_ms: u64,
    /// Peak input current of the current session, µA.
    pub current_peak_iin_ua: u32,
    /// Whether fast charging occurred during the current session.
    pub current_fast: u8,
    /// Reserved.
    pub reserved: [u8; 3],
    /// Duration of the last completed session, ms.
    pub last_ms: u64,
    /// Peak current of the last completed session, µA.
    pub last_peak_iin_ua: u32,
    /// Peak temperature of the last session, tenths of °C.
    pub last_peak_temp_dc: i32,
    /// Whether fast charging occurred in the last session.
    pub last_fast: u8,
    /// Reserved.
    pub reserved2: [u8; 3],
}

/// One telemetry sample in the [`IOCTL_LN8000_GET_SAMPLES`] buffer.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000Sample {
    /// Timestamp, ms.
    pub ts_ms: u64,
    /// Battery voltage, µV.
    pub vbat_uv: u32,
    /// Input voltage, µV.
    pub vbus_uv: u32,
    /// Input current, µA.
    pub iin_ua: u32,
    /// Die temperature, tenths of °C.
    pub die_temp_dc: i32,
    /// Operating mode.
    pub op_mode: u8,
    /// Whether input power is present.
    pub input_present: u8,
    /// Reserved.
    pub reserved: [u8; 2],
}

/// Request for [`IOCTL_LN8000_GET_SAMPLES`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000SamplesRequest {
    /// How many samples to return (bounded by the client buffer size).
    pub count: u32,
    /// How many samples were actually written.
    pub available: u32,
    /// First sample in the buffer (the rest follow contiguously).
    pub first: Ln8000Sample,
}

/// Request for [`IOCTL_LN8000_RUN_HVDCP`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000HvdcpRequest {
    /// 1 = run negotiate; 0 = report last soft state only (no bus).
    pub command: u8,
    /// Last APSD_STATUS after a run.
    pub apsd_status: u8,
    /// Last APSD_RESULT after a run.
    pub apsd_result: u8,
    /// Soft pulse count.
    pub pulse_cnt: u8,
    /// Machine phase code (`HvdcpPhase`).
    pub phase: u32,
    /// Target VBUS (µV), `2*VBAT+200mV`.
    pub target_vbus_uv: u32,
    /// Estimated adapter VBUS from soft pulse count (µV).
    pub estimated_vbus_uv: u32,
    /// 0 = ok; -10 = no SUPERUSER+Usbin; -11 open fail; -12 SPMI; -13 APSD timeout; -14 not QC.
    pub error_code: i32,
}
