//! Register map of the SMB charger USBIN peripheral.
//!
//! The values are taken from the reference Android driver
//! (`drivers/power/supply/qcom/smb5-reg.h`, `smb-reg.h`) branch 16.0 for `nabu`,
//! so that behaviour under Windows matches behaviour under Android.

/// Register address in the peripheral space (16 bits).
pub type RegAddr = u16;

/// USBIN peripheral base.
pub const USBIN_BASE: RegAddr = 0x1300;

/// `APSD_STATUS`: state of the adapter detection state machine.
pub const APSD_STATUS: RegAddr = USBIN_BASE + 0x07;
/// "detection complete" bit.
pub const APSD_DTC_STATUS_DONE: u8 = 1 << 0;
/// "a Quick Charge capable adapter is attached" bit.
pub const QC_CHARGER: u8 = 1 << 1;
/// "adapter attached too slowly" bit.
pub const SLOW_PLUGIN_TIMEOUT: u8 = 1 << 5;
/// "HVDCP check did not fit the timeout" bit.
pub const HVDCP_CHECK_TIMEOUT: u8 = 1 << 6;
/// Eighth bit of `APSD_STATUS` (vendor-defined, kept for completeness).
pub const APSD_STATUS_7: u8 = 1 << 7;

/// `APSD_RESULT_STATUS`: detection result (adapter type).
pub const APSD_RESULT_STATUS: RegAddr = USBIN_BASE + 0x08;
/// Mask of the significant result bits.
pub const APSD_RESULT_STATUS_MASK: u8 = 0b0111_1111;
/// Service eighth bit of the result.
pub const APSD_RESULT_STATUS_7: u8 = 1 << 7;

/// `QC_CHANGE_STATUS`: Quick Charge negotiation state.
pub const QC_CHANGE_STATUS: RegAddr = USBIN_BASE + 0x09;

/// `USBIN_CMD_IL`: input control (including suspend).
pub const USBIN_CMD_IL: RegAddr = USBIN_BASE + 0x40;
/// Input suspend bit.
pub const USBIN_SUSPEND: u8 = 1 << 0;

/// `CMD_APSD`: commands to the detection state machine.
pub const CMD_APSD: RegAddr = USBIN_BASE + 0x41;
/// Detection rerun bit.
pub const APSD_RERUN: u8 = 1 << 0;

/// `CMD_ICL_OVERRIDE`: forced input current limit.
pub const CMD_ICL_OVERRIDE: RegAddr = USBIN_BASE + 0x42;
/// Forced limit enable bit.
pub const ICL_OVERRIDE: u8 = 1 << 0;
/// "apply the limit after APSD completes" bit.
pub const ICL_OVERRIDE_AFTER_APSD: u8 = 1 << 4;

/// `CMD_HVDCP_2`: HVDCP2 mode control.
pub const CMD_HVDCP_2: RegAddr = USBIN_BASE + 0x43;

/// `USBIN_ADAPTER_ALLOW_OVERRIDE`: override of the allowed adapter types.
pub const USBIN_ADAPTER_ALLOW_OVERRIDE: RegAddr = USBIN_BASE + 0x44;

/// `USB_CMD_PULLDOWN`: control of the D+/D− pull-downs.
pub const USB_CMD_PULLDOWN: RegAddr = USBIN_BASE + 0x45;

/// `HVDCP_PULSE_COUNT_MAX`: voltage selection for QC2.
pub const HVDCP_PULSE_COUNT_MAX: RegAddr = USBIN_BASE + 0x5B;
/// QC2 voltage selection mask (bits 7:6).
pub const QC2_VOLTAGE_MASK: u8 = 0b1100_0000;

/// `USBIN_ICL_OPTIONS`: extra current limit options.
pub const USBIN_ICL_OPTIONS: RegAddr = USBIN_BASE + 0x66;

/// `USBIN_CURRENT_LIMIT_CFG`: input current limit code (100 mA grid).
pub const USBIN_CURRENT_LIMIT_CFG: RegAddr = USBIN_BASE + 0x70;

/// `APSD_RESULT_STATUS` pattern for SDP (standard port).
pub const PATTERN_SDP: u8 = 1 << 0;
/// Pattern for OCP (other port).
pub const PATTERN_OCP: u8 = 1 << 1;
/// Pattern for CDP (charging and data port).
pub const PATTERN_CDP: u8 = 1 << 2;
/// Pattern for DCP (charging-only port).
pub const PATTERN_DCP: u8 = 1 << 3;
/// Pattern for FLOAT (non-standard source).
pub const PATTERN_FLOAT: u8 = 1 << 4;
/// Quick Charge 2.0 bit in the pattern.
pub const PATTERN_QC_2P0: u8 = 1 << 5;
/// Quick Charge 3.0 bit in the pattern.
pub const PATTERN_QC_3P0: u8 = 1 << 6;

/// Pattern for HVDCP2: DCP plus the QC2.0 flag.
pub const PATTERN_HVDCP2: u8 = PATTERN_DCP | PATTERN_QC_2P0;
/// Pattern for HVDCP3: DCP plus the QC3.0 flag.
pub const PATTERN_HVDCP3: u8 = PATTERN_DCP | PATTERN_QC_3P0;
