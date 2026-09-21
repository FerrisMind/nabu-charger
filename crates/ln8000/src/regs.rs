//! LN8000 register map and value encoding.
//!
//! Source: the driver from the reference Android sources:
//! `drivers/power/supply/ti/ln8000_charger.h` and `ln8000_charger.c` (GPL,
//! `Lion Semiconductor` / `XiaoMi`, 2021). All the encoding formulas repeat
//! that driver's functions so that behaviour under Windows matches Android.

/// LN8000 register address (single byte).
pub type RegAddr = u8;

// --- Status registers ---------------------------------------------------

/// Device identifier: expected value is [`DEVICE_ID_VALUE`].
pub const DEVICE_ID: RegAddr = 0x00;
/// Expected response to a read of [`DEVICE_ID`].
pub const DEVICE_ID_VALUE: u8 = 0x42;
/// Interrupt mask.
pub const INT1: RegAddr = 0x01;
/// Interrupt enable.
pub const INT1_MSK: RegAddr = 0x02;
/// System status: operating mode and active regulation loops.
pub const SYS_STS: RegAddr = 0x03;
/// Status of the protections (temperatures, reverse current).
pub const SAFETY_STS: RegAddr = 0x04;
/// First fault group.
pub const FAULT1_STS: RegAddr = 0x05;
/// Second fault group.
pub const FAULT2_STS: RegAddr = 0x06;
/// Current status.
pub const CURR1_STS: RegAddr = 0x07;
/// LDO and charge-termination status.
pub const LDO_STS: RegAddr = 0x08;
/// First ADC result register (channels 1..10 follow in sequence).
pub const ADC_FIRST_STS: RegAddr = 0x09;
/// Last ADC result register.
pub const ADC_LAST_STS: RegAddr = 0x12;

// --- Control registers --------------------------------------------------

/// Input current limit.
pub const IIN_CTRL: RegAddr = 0x1B;
/// Regulation loop control.
pub const REGULATION_CTRL: RegAddr = 0x1C;
/// Power control.
pub const PWR_CTRL: RegAddr = 0x1D;
/// Operating mode (standby / bypass / switching).
pub const SYS_CTRL: RegAddr = 0x1E;
/// LDO control.
pub const LDO_CTRL: RegAddr = 0x1F;
/// Surge protection, input overvoltage threshold.
pub const GLITCH_CTRL: RegAddr = 0x20;
/// Protection enable.
pub const FAULT_CTRL: RegAddr = 0x21;
/// NTC threshold.
pub const NTC_CTRL: RegAddr = 0x22;
/// ADC control (mode, hibernation delay, high NTC bits).
pub const ADC_CTRL: RegAddr = 0x23;
/// ADC configuration.
pub const ADC_CFG: RegAddr = 0x24;
/// Auto-recovery after faults.
pub const RECOVERY_CTRL: RegAddr = 0x25;
/// Timers (watchdog and ADC pause).
pub const TIMER_CTRL: RegAddr = 0x26;

/// ADC update pause bit in the [`TIMER_CTRL`] register (bit 1).
///
/// While the bit is set, conversions do not rewrite the results. Without the
/// pause, the two bytes of a sample can belong to different conversions and the
/// temperature comes out as garbage, and protection decisions rest on it. The
/// reference driver does the same: sets the bit, reads the pair, clears the bit.
pub const TIMER_CTRL_PAUSE_ADC: u8 = 1 << 1;
/// Pulse bit to clear latched fault/status (Android `ln8000_check_status`).
pub const TIMER_CTRL_CLEAR_LATCH: u8 = 1 << 2;
/// `FAULT_CTRL` bit: disable hardware `VIN_OV` (needed for QC 9–12 V bus).
pub const FAULT_CTRL_DISABLE_VIN_OV: u8 = 1 << 2;
/// `FAULT_CTRL` bit: disable hardware `VAC_UV` (`LN8000_BIT_DISABLE_VAC_UV`).
/// Required for saggy 5 V bricks (TA200) where Vin dips near Vbat under load.
pub const FAULT_CTRL_DISABLE_VAC_UV: u8 = 1 << 3;
/// `FAULT_CTRL` bit: disable hardware `VAC_OV` (`LN8000_BIT_DISABLE_VAC_OV`).
pub const FAULT_CTRL_DISABLE_VAC_OV: u8 = 1 << 4;
/// `FAULT_CTRL` bit: disable hardware `VBAT_OV` (`LN8000_BIT_DISABLE_VBAT_OV`).
/// Soft-mask near float so a latched OV does not block `volt_qual` / mode entry.
pub const FAULT_CTRL_DISABLE_VBAT_OV: u8 = 1 << 5;
/// Mask used before 5 V / TA200-class bypass: UV/OV that latch `FAULT1=0x21`.
pub const FAULT_CTRL_MASK_5V_BYPASS: u8 = FAULT_CTRL_DISABLE_VIN_OV
    | FAULT_CTRL_DISABLE_VAC_UV
    | FAULT_CTRL_DISABLE_VAC_OV
    | FAULT_CTRL_DISABLE_VBAT_OV;

/// Thresholds.
pub const THRESHOLD_CTRL: RegAddr = 0x27;
/// Charge target voltage (float).
pub const V_FLOAT_CTRL: RegAddr = 0x28;
/// Initialisation flag and charge control.
pub const CHARGE_CTRL: RegAddr = 0x29;
/// Service register (unlock, soft-reset).
pub const LION_CTRL: RegAddr = 0x30;
/// Built-in controller operations, group 1.
pub const BC_OP_1: RegAddr = 0x41;
/// Built-in controller operations, group 2 (soft-reset).
pub const BC_OP_2: RegAddr = 0x42;
/// Built-in controller status, group A.
pub const BC_STS_A: RegAddr = 0x49;
/// Built-in controller status, group E.
pub const BC_STS_E: RegAddr = 0x4D;

/// Value to unlock the service registers.
pub const LION_CTRL_UNLOCK: u8 = 0xC6;

// --- SYS_STS bits -------------------------------------------------------

/// Input current regulation loop is active.
pub const SYS_STS_IIN_LOOP: u8 = 1 << 7;
/// Charge voltage regulation loop is active.
pub const SYS_STS_VFLOAT_LOOP: u8 = 1 << 6;
/// Bypass mode (1:1) is enabled.
pub const SYS_STS_BYPASS_ENABLED: u8 = 1 << 3;
/// Switching mode (2:1) is enabled.
pub const SYS_STS_SWITCHING_ENABLED: u8 = 1 << 2;
/// The device is in standby.
pub const SYS_STS_STANDBY: u8 = 1 << 1;
/// The device is off.
pub const SYS_STS_SHUTDOWN: u8 = 1 << 0;

// --- SYS_CTRL bits ------------------------------------------------------

/// Standby enable bit.
pub const SYS_CTRL_STANDBY_EN: u8 = 1 << 3;
/// Reverse current detection bit.
pub const SYS_CTRL_REV_IIN_DET: u8 = 1 << 2;
/// Bit enabling 1:1 mode (bypass).
pub const SYS_CTRL_EN_1TO1: u8 = 1 << 0;

// --- SAFETY_STS / FAULT / LDO bits --------------------------------------

/// Maximum die temperature reached.
pub const SAFETY_TEMP_MAX: u8 = 1 << 6;
/// Temperature regulation is active.
pub const SAFETY_TEMP_REGULATION: u8 = 1 << 5;
/// NTC alarm fired.
pub const SAFETY_NTC_ALARM: u8 = 1 << 4;
/// NTC protection fired.
pub const SAFETY_NTC_SHUTDOWN: u8 = 1 << 3;
/// Reverse input current detected.
pub const SAFETY_REV_IIN: u8 = 1 << 2;

/// Watchdog timer expired.
pub const FAULT1_WATCHDOG: u8 = 1 << 7;
/// Battery overvoltage.
pub const FAULT1_VBAT_OV: u8 = 1 << 6;
/// Power supply disconnected.
pub const FAULT1_VAC_UNPLUG: u8 = 1 << 4;
/// Input overvoltage (VAC).
pub const FAULT1_VAC_OV: u8 = 1 << 3;
/// VIN overvoltage.
pub const FAULT1_VIN_OV: u8 = 1 << 1;

/// Group flag of the "voltage" faults in `FAULT1` (bits 6:0).
///
/// The vendor tests the group as a whole (`LN8000_MASK_VFAULTS`, `.h:65`) rather
/// than individual bits: `volt_qual = !(FAULT1 & 0x7F)` - the input counts as
/// good only when bits 6:0 are completely clean (`.c:592-604`). Bits 5, 2 and 0
/// are unnamed in the public driver, so we give them no names of our own: a live
/// `FAULT1=0x21` is two unnamed bits of the group, and any name for them would be
/// an invention.
pub const FAULT1_VFAULTS_MASK: u8 = 0x7F;

/// Input overcurrent detected.
pub const FAULT2_IIN_OC: u8 = 1 << 7;

/// Second stage of the vendor input-valid test: bit 5 in `FAULT2`.
///
/// Checked only when the first stage (`FAULT1`) is clean **and** charging is
/// enabled (`ln8000_check_status`, `.c:592-604`).
pub const FAULT2_VOLT_FAULT: u8 = 1 << 5;

/// Charge complete.
pub const LDO_CHARGE_TERM: u8 = 1 << 5;
/// Recharge required.
pub const LDO_RECHARGE: u8 = 1 << 4;

// --- Numeric constants --------------------------------------------------

/// Minimum charge voltage encoding, µV.
pub const VBAT_FLOAT_MIN_UV: u32 = 3_725_000;
/// Maximum charge voltage encoding, µV.
pub const VBAT_FLOAT_MAX_UV: u32 = 5_000_000;
/// Charge voltage encoding step, µV.
pub const VBAT_FLOAT_STEP_UV: u32 = 5_000;

/// Minimum input current (per the driver documentation), µA.
pub const IIN_MIN_UA: u32 = 500_000;
/// Input current encoding step, µA.
pub const IIN_STEP_UA: u32 = 50_000;
/// Maximum of the input current field (7 bits), µA.
pub const IIN_MAX_UA: u32 = IIN_STEP_UA * 0x7F;

/// Input overvoltage threshold: 6.5 V.
pub const VAC_OVP_6V5: u8 = 0x0;
/// Input overvoltage threshold: 11 V.
pub const VAC_OVP_11V: u8 = 0x1;
/// Input overvoltage threshold: 12 V.
pub const VAC_OVP_12V: u8 = 0x2;
/// Input overvoltage threshold: 13 V.
pub const VAC_OVP_13V: u8 = 0x3;

/// NTC temperature protection setting: −16 LSB (≈ −4.3 °C).
pub const NTC_SHUTDOWN_CFG: u8 = 2;
/// Default NTC alarm threshold (≈ +40 °C).
pub const NTC_ALARM_DEFAULT: u16 = 226;

/// Threshold register value from the driver (`THRESHOLD_CTRL`).
pub const THRESHOLD_CTRL_DEFAULT: u8 = 0x0E;
/// Restart delay after reset, ms (in the driver - `msleep(5 * 2)`).
pub const SOFT_RESET_DELAY_MS: u64 = 10;
