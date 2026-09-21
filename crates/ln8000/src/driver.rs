//! LN8000 charge pump driver: chip probe, initialisation, modes, status.
//!
//! The logic repeats `ln8000_init_device()` and `ln8000_change_opmode()` from the
//! reference Android driver, so that under Windows the device is configured the
//! same way as under Android.
//!
//! The driver does not block and does not sleep: everything that takes time (the
//! post-reset delay, watchdog timer servicing) is the caller's job.
//!
//! # Example
//!
//! ```
//! use ln8000::testkit::MockPumpBus;
//! use ln8000::{OpMode, Pump, PumpConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let bus = MockPumpBus::new();
//! let mut pump = Pump::open(bus, PumpConfig::default())?;
//! pump.configure()?;
//! assert_eq!(pump.enable_switching()?, OpMode::Switching);
//! let status = pump.status()?;
//! assert_eq!(status.op_mode, OpMode::Switching);
//! # Ok(())
//! # }
//! ```

use crate::encoding::{
    AdcHibernateDelay, AdcMode, NABU_VBAT_FLOAT_UV, OpMode, POR_VIN_TOLERANCE_UV,
    VBAT_TAPER_IIN_UA, WatchdogPeriod, bypass_allowed_by_vin, charge_mode, decode_iin_limit,
    encode_iin_limit, encode_ntc_alarm, encode_vac_ovp, encode_vbat_float, soft_float_for_vbat,
    vbat_near_float_with_vin,
};
use crate::error::{BusError, PumpError};
use crate::regs;
use crate::status::{AdcChannel, Status};
use crate::transport::RegisterBus;

/// Driver settings.
///
/// The protection flags deliberately repeat the Device Tree names: they are
/// independent hardware switches of the pump, not "boolean soup" from app logic.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpConfig {
    /// Charge target voltage, µV (default 4.44 V).
    pub vbat_float_uv: u32,
    /// Input overvoltage threshold, µV (default 9.5 V).
    pub vac_ovp_uv: u32,
    /// Input current limit, µA (default 2 A).
    pub iin_limit_ua: u32,
    /// NTC alarm threshold (10 bits, default 226 ≈ +40 °C).
    pub ntc_alarm_cfg: u16,
    /// Whether to enable the watchdog timer.
    pub watchdog_enabled: bool,
    /// Watchdog timer period.
    pub watchdog_period: WatchdogPeriod,
    /// Whether to enable auto-recovery after faults.
    pub auto_recovery: bool,
    /// Verify every write by reading it back.
    pub verify_writes: bool,
    /// How many times to retry the operation on a bus failure.
    pub max_bus_retries: u8,
    // --- protection flags (names repeat the tablet Device Tree) ---
    //
    // In the Android scheme the main charger (SMB) handles regulation and
    // temperature, while the LN8000 works as a 2:1 cascade, so the DTS *disables*
    // the pump's own protections. We do not hard-code the choice: which variant
    // is needed under Windows (where the main stack may not charge) is set by the profile.
    /// Disable charge voltage regulation (`vbat-reg-disable`).
    pub vbat_reg_disabled: bool,
    /// Disable input overcurrent protection (`iin-ocp-disable`).
    pub iin_ocp_disabled: bool,
    /// Disable input current regulation (`iin-reg-disable`).
    pub iin_reg_disabled: bool,
    /// Disable die temperature protection (`tdie-prot-disable`).
    pub tdie_prot_disabled: bool,
    /// Disable die temperature regulation (`tdie-reg-disable`).
    pub tdie_reg_disabled: bool,
    /// Disable bus temperature monitoring (`tbus-mon-disable`).
    pub tbus_mon_disabled: bool,
    /// Disable battery temperature monitoring (`tbat-mon-disable`).
    pub tbat_mon_disabled: bool,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            // Defaults come from `ln8000_charger.h`
            // (`LN8000_BAT_OVP_DEFAULT`, `LN8000_BUS_OVP_DEFAULT`,
            // `LN8000_IIN_CFG_DEFAULT`, `LN8000_NTC_ALARM_CFG_DEFAULT`).
            // Android nabu: bat_ovp 4560 mV → V_FLOAT ≈ 4470 mV (ovp = float×1.02).
            vbat_float_uv: NABU_VBAT_FLOAT_UV,
            vac_ovp_uv: 9_500_000,
            iin_limit_ua: 2_000_000,
            ntc_alarm_cfg: regs::NTC_ALARM_DEFAULT,
            watchdog_enabled: false,
            watchdog_period: WatchdogPeriod::Sec10,
            auto_recovery: false,
            verify_writes: true,
            max_bus_retries: 2,
            vbat_reg_disabled: false,
            iin_ocp_disabled: false,
            iin_reg_disabled: false,
            tdie_prot_disabled: false,
            tdie_reg_disabled: false,
            tbus_mon_disabled: false,
            tbat_mon_disabled: false,
        }
    }
}

impl PumpConfig {
    /// Tablet Device Tree profile: the pump's own protections are disabled.
    ///
    /// The flags are taken from `nabu-sm8150.dtsi` (`tdie-prot-disable`,
    /// `iin-ocp-disable`, `iin-reg-disable`, `tdie-reg-disable`,
    /// `vbat-reg-disable`, `tbus-mon-disable`, `tbat-mon-disable`).
    /// This is the set Xiaomi considers correct for nabu: the main charger
    /// handles regulation, and the pump works as a cascade.
    #[must_use]
    pub fn for_nabu_dts() -> Self {
        Self {
            vbat_reg_disabled: true,
            iin_ocp_disabled: true,
            iin_reg_disabled: true,
            tdie_prot_disabled: true,
            tdie_reg_disabled: true,
            tbus_mon_disabled: true,
            tbat_mon_disabled: true,
            ..Self::default()
        }
    }

    /// Applies a parameter from the registry to the profile.
    ///
    /// The names match the INF parameters (`HKR, Parameters, ...`), so whoever
    /// installs the driver can change the thresholds **without a rebuild**.
    ///
    /// Returns `true` if the parameter is known and accepted. `false` means the
    /// name is unknown or the value is outside the allowed bounds - the profile is
    /// unchanged, so a wrong value cannot silently corrupt the settings.
    ///
    /// The `TelemetryMs` parameter is not handled here: the timer period is the driver's.
    #[must_use]
    pub fn apply_parameter(&mut self, name: &str, value: u32) -> bool {
        match name {
            // The input current bounds are checked by the same encoding that goes
            // to the chip: out of range it returns an error and the value is rejected.
            "IinLimitUa" => {
                if encode_iin_limit(value).is_err() {
                    return false;
                }
                self.iin_limit_ua = value;
                true
            }
            // Bounds from the reference header: `LN8000_VBAT_FLOAT_MIN/MAX`.
            "VbatFloatUv" => {
                if !(3_725_000..=5_000_000).contains(&value) {
                    return false;
                }
                self.vbat_float_uv = value;
                true
            }
            // `LN8000_VAC_OVP_6P5V` … `_13V`.
            "VacOvpUv" => {
                if !(6_500_000..=13_000_000).contains(&value) {
                    return false;
                }
                self.vac_ovp_uv = value;
                true
            }
            // The NTC alarm threshold is 10 bits.
            "NtcAlarmCfg" => {
                if value > 0x03FF {
                    return false;
                }
                self.ntc_alarm_cfg = u16::try_from(value).unwrap_or(self.ntc_alarm_cfg);
                true
            }
            "WatchdogEnabled" => {
                self.watchdog_enabled = value != 0;
                true
            }
            // How many times to retry the operation on a bus failure.
            "BusRetryCount" => {
                if value > 8 {
                    return false;
                }
                self.max_bus_retries = u8::try_from(value).unwrap_or(self.max_bus_retries);
                true
            }
            // 0 - as in the tablet Device Tree (the pump's protections are off),
            // 1 - with the loops enabled. The choice is made at installation time.
            "ProtectionProfile" => {
                let template = match value {
                    0 => Self::for_nabu_dts(),
                    1 => Self::protective(),
                    _ => return false,
                };
                self.vbat_reg_disabled = template.vbat_reg_disabled;
                self.iin_ocp_disabled = template.iin_ocp_disabled;
                self.iin_reg_disabled = template.iin_reg_disabled;
                self.tdie_prot_disabled = template.tdie_prot_disabled;
                self.tdie_reg_disabled = template.tdie_reg_disabled;
                self.tbus_mon_disabled = template.tbus_mon_disabled;
                self.tbat_mon_disabled = template.tbat_mon_disabled;
                true
            }
            _ => false,
        }
    }

    /// Profile with the pump's protections enabled.
    ///
    /// The variant for the case where the main stack under Windows does not charge
    /// the battery and the pump has to regulate itself. It differs from
    /// [`Self::default()`] only in listing every field: handy for a hardware run.
    #[must_use]
    pub fn protective() -> Self {
        Self {
            vbat_reg_disabled: false,
            iin_ocp_disabled: false,
            iin_reg_disabled: false,
            tdie_prot_disabled: false,
            tdie_reg_disabled: false,
            tbus_mon_disabled: false,
            tbat_mon_disabled: false,
            ..Self::default()
        }
    }

    /// Profile for working through the charge pump from a Quick Charge 3.5 class B adapter.
    ///
    /// The thresholds match `BUS_OVP_FOR_QC`, `BUS_OCP_FOR_QC3P5_CLASS_B`
    /// from `ln8000_charger.h`.
    ///
    /// Android DTS disables LN8000 VFLOAT/IIN loops because SMB does CV. Under
    /// Windows the PEIC path owns charging — keep VFLOAT + IIN regulation on so
    /// VBAT cannot idle at `Vin/2` (~4.78 V) and starve Iin.
    #[must_use]
    pub fn for_qc35_class_b() -> Self {
        Self {
            vac_ovp_uv: 13_000_000,
            iin_limit_ua: 3_500_000 - 700_000,
            vbat_reg_disabled: false,
            iin_reg_disabled: false,
            // Thermal / OCP monitors still follow nabu DTS (SMB-era defaults).
            iin_ocp_disabled: true,
            tdie_prot_disabled: true,
            tdie_reg_disabled: true,
            tbus_mon_disabled: true,
            tbat_mon_disabled: true,
            ..Self::default()
        }
    }

    /// Profile for testing 2:1 mode without high voltage.
    #[must_use]
    pub fn conservative() -> Self {
        Self {
            vac_ovp_uv: 6_500_000,
            iin_limit_ua: 1_000_000,
            ..Self::default()
        }
    }
}

/// Driver session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpState {
    /// Session not open.
    Closed,
    /// Chip identified.
    Probed,
    /// Thresholds and protections configured.
    Configured,
    /// 2:1 mode enabled.
    Switching,
    /// Device is in fault.
    Faulted,
}

impl PumpState {
    /// State name for the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Probed => "probed",
            Self::Configured => "configured",
            Self::Switching => "switching",
            Self::Faulted => "faulted",
        }
    }
}

/// Charge pump driver over an abstract I²C bus.
#[derive(Debug)]
pub struct Pump<T: RegisterBus> {
    bus: T,
    config: PumpConfig,
    state: PumpState,
    op_mode: OpMode,
    /// Stage the 5 V fallback reached in the last attempt: `0` - not requested,
    /// `1` - a clean mode write, `2` - POR with the profile, `3` - the fault mask.
    /// See [`Self::bypass_stage`].
    bypass_stage: u8,
    /// Input on which the latch was already cleared via POR.
    ///
    /// The reference does POR **once** and the chip obeys afterwards; repeating the
    /// reset on every attempt is not allowed - it jerks the charge and erases state
    /// the chip may have latched legitimately. Until Vin has changed by more than
    /// [`POR_VIN_TOLERANCE_UV`], no second POR is done.
    por_vin_uv: Option<u32>,
    /// How many writes and reads have been performed (for reports).
    writes: u32,
    reads: u32,
}

impl<T: RegisterBus> Pump<T> {
    /// Opens a session: checks the link and the device identifier.
    ///
    /// # Errors
    ///
    /// * [`PumpError::Bus`] - the bus is unavailable.
    /// * [`PumpError::WrongDeviceId`] - the response is not [`regs::DEVICE_ID_VALUE`].
    pub fn open(mut bus: T, config: PumpConfig) -> Result<Self, PumpError> {
        let _ = bus.reset();
        let id = bus.read(regs::DEVICE_ID)?;
        if id != regs::DEVICE_ID_VALUE {
            return Err(PumpError::WrongDeviceId { got: id });
        }
        Ok(Self {
            bus,
            config,
            state: PumpState::Probed,
            op_mode: OpMode::Unknown,
            bypass_stage: 0,
            por_vin_uv: None,
            writes: 0,
            reads: 1,
        })
    }

    /// Current session state.
    #[must_use]
    pub const fn state(&self) -> PumpState {
        self.state
    }

    /// Last known mode.
    #[must_use]
    pub const fn op_mode(&self) -> OpMode {
        self.op_mode
    }

    /// Stage of the 5 V fallback reached by the last attempt.
    ///
    /// `0` - 1:1 mode was not requested; `1` - a clean `SYS_CTRL` write without
    /// `FAULT_CTRL` edits (that one worked on 17.09); `2` - POR (`soft_reset`,
    /// delay, `configure`, mode write); `3` - the `.627` add-on with the UV/OV mask
    /// and the latch-clear pulse. The vendor documents neither stage 2 nor stage 3:
    /// 2 is confirmed by live runs, 3 is not.
    #[must_use]
    pub const fn bypass_stage(&self) -> u8 {
        self.bypass_stage
    }

    /// Whether the POR budget of the current input has been spent: `true` if the
    /// latch on this Vin was already cleared. This mark shows whether the driver
    /// is on its first attempt or has hit the failure and waits for an adapter change.
    #[must_use]
    pub const fn por_spent(&self) -> bool {
        self.por_vin_uv.is_some()
    }

    /// Read-only bus access: diagnostics and tests.
    #[must_use]
    pub const fn bus(&self) -> &T {
        &self.bus
    }

    /// Mutable bus access: preparing states in diagnostics and tests.
    pub fn bus_mut(&mut self) -> &mut T {
        &mut self.bus
    }

    /// Bus name.
    #[must_use]
    pub fn bus_name(&self) -> &'static str {
        self.bus.name()
    }

    /// How many writes and reads have been performed.
    #[must_use]
    pub const fn counters(&self) -> (u32, u32) {
        (self.writes, self.reads)
    }

    /// Session settings.
    #[must_use]
    pub const fn config(&self) -> &PumpConfig {
        &self.config
    }

    /// Configures the thresholds and protections.
    ///
    /// The order repeats `ln8000_init_device()`:
    ///
    /// 1. charge voltage (`V_FLOAT_CTRL`);
    /// 2. input overvoltage threshold (`GLITCH_CTRL[3:2]`);
    /// 3. input current limit (`IIN_CTRL[6:0]`);
    /// 4. NTC threshold (`NTC_CTRL` + `ADC_CTRL[1:0]`);
    /// 5. NTC protection configuration (`REGULATION_CTRL[3:2]`);
    /// 6. auto-recovery (`RECOVERY_CTRL[7:4]`);
    /// 7. enabling protections and regulation loops;
    /// 8. switching to standby;
    /// 9. watchdog timer, ADC and temperature monitors;
    /// 10. software initialisation mark and thresholds.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::Bus`], [`PumpError::OutOfRange`] - bus or value failure.
    pub fn configure(&mut self) -> Result<(), PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }

        // 1. charge voltage
        let vfloat = encode_vbat_float(self.config.vbat_float_uv);
        self.write_verified(regs::V_FLOAT_CTRL, vfloat, "vbat_float")?;

        // 2. input overvoltage threshold
        let vac = encode_vac_ovp(self.config.vac_ovp_uv);
        self.update(regs::GLITCH_CTRL, 0x03 << 2, vac << 2, "vac_ovp")?;

        // 3. input current limit (as in the driver: limit = OCP - 700 mA)
        let iin_code = encode_iin_limit(self.config.iin_limit_ua)?;
        self.update(regs::IIN_CTRL, 0x7F, iin_code, "iin_limit")?;

        // 4. NTC threshold: the low bits in NTC_CTRL, the high bits in ADC_CTRL
        let (low, high) = encode_ntc_alarm(self.config.ntc_alarm_cfg);
        self.write_verified(regs::NTC_CTRL, low, "ntc_alarm_low")?;
        self.update(regs::ADC_CTRL, 0x03, high, "ntc_alarm_high")?;

        // 5. NTC temperature protection configuration
        self.update(
            regs::REGULATION_CTRL,
            0x03 << 2,
            regs::NTC_SHUTDOWN_CFG << 2,
            "ntc_shutdown_cfg",
        )?;

        // 6. auto-recovery and the bus and battery temperature monitors (bits 1:0).
        let recovery = if self.config.auto_recovery {
            0xF0
        } else {
            0x00
        };
        let monitors = u8::from(!self.config.tbus_mon_disabled) << 1
            | u8::from(!self.config.tbat_mon_disabled);
        self.update(
            regs::RECOVERY_CTRL,
            0xF0 | 0b11,
            recovery | monitors,
            "recovery_and_monitors",
        )?;

        // 7. protections and regulation loops - according to the profile flags.
        self.configure_protections()?;

        // 8. switch to standby
        self.set_op_mode(OpMode::Standby)?;
        self.update(regs::FAULT_CTRL, 1 << 4, 0, "enable_vac_ov")?;

        // 9. watchdog timer and ADC
        let wdt = if self.config.watchdog_enabled {
            1u8 << 7
        } else {
            0
        };
        self.update(regs::TIMER_CTRL, 1 << 7, wdt, "watchdog_enable")?;
        self.update(
            regs::TIMER_CTRL,
            0x03 << 5,
            self.config.watchdog_period.code() << 5,
            "watchdog_period",
        )?;
        self.update(
            regs::ADC_CTRL,
            0x07 << 5,
            AdcMode::Shutdown.code() << 5,
            "adc_off_before_config",
        )?;
        self.update(
            regs::ADC_CTRL,
            0x03 << 3,
            AdcHibernateDelay::Sec4.code() << 3,
            "adc_hibernate_delay",
        )?;
        // All ADC channels (like `ln8000_set_adc_ch(ALL, true)`).
        self.write_verified(regs::ADC_CFG, 0x3E, "adc_channels")?;
        self.update(
            regs::ADC_CTRL,
            0x07 << 5,
            AdcMode::AutoHibernate.code() << 5,
            "adc_auto",
        )?;

        // 10. initialisation mark and thresholds
        self.update(regs::CHARGE_CTRL, 1 << 7, 1 << 7, "sw_init_marker")?;
        self.write_verified(
            regs::THRESHOLD_CTRL,
            regs::THRESHOLD_CTRL_DEFAULT,
            "thresholds",
        )?;

        self.state = PumpState::Configured;
        Ok(())
    }

    /// Writes the protection and regulation loop bits according to the profile flags.
    ///
    /// In the tablet Device Tree some of the pump's own protections are disabled
    /// (`tdie-prot-disable`, `iin-ocp-disable`, `iin-reg-disable`,
    /// `tdie-reg-disable`, `vbat-reg-disable`): in the Android scheme regulation is
    /// handled by the main SMB charger, and the pump works as a 2:1 cascade. Here it
    /// follows the profile rather than being hard-coded.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::Bus`] - bus failure.
    fn configure_protections(&mut self) -> Result<(), PumpError> {
        let cfg = self.config;
        self.update(regs::FAULT_CTRL, 1 << 5, 0, "enable_vbat_ovp")?;
        // `FAULT_CTRL` IIN_OCP is switched off **always**, regardless of the profile:
        // the tablet's vendor DT does not keep it (`ln8000_charger,
        // iin-ocp-disable`, `nabu-sm8150.dtsi:239`), and `bus-ocp-threshold = 3750`
        // mA is set there only as an alarm. When enabled, the protection latches
        // `FAULT2_IIN_OC` on the very first entry into 2:1 - live measurement 19.09
        // 11:10: bus 8.256 V, `PostHvdcpMode = 3`, then `FAULT2 = 0x80`, `SuMode = 1`,
        // 39.1 mA, `EngageState = 0` - no charging at all.
        // The profile still controls the loops (`iIN_REG`/`VFLOAT`), but not
        // this latch.
        self.update(regs::FAULT_CTRL, 1 << 6, 1 << 6, "iin_ocp_off_nabu_dts")?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 5,
            u8::from(cfg.vbat_reg_disabled) << 5,
            "vfloat_loop",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 7,
            u8::from(!cfg.vbat_reg_disabled) << 7,
            "vfloat_loop_int",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 4,
            u8::from(cfg.iin_reg_disabled) << 4,
            "iin_loop",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 6,
            u8::from(!cfg.iin_reg_disabled) << 6,
            "iin_loop_int",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 2,
            u8::from(!cfg.tdie_prot_disabled) << 2,
            "tdie_prot",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 1,
            u8::from(!cfg.tdie_reg_disabled) << 1,
            "tdie_regulation",
        )?;
        self.update(regs::SYS_CTRL, 1 << 2, 0, "disable_reverse_current")
    }

    /// Enables 2:1 mode (switching) and checks that the device accepted it.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::ModeNotReached`] - `SYS_STS` did not confirm the mode.
    pub fn enable_switching(&mut self) -> Result<OpMode, PumpError> {
        self.set_op_mode(OpMode::Switching)?;
        let status = self.settle_and_read_status()?;
        if status.op_mode != OpMode::Switching {
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Switching.code(),
                raw_status: status.sys_sts,
            });
        }
        self.state = PumpState::Switching;
        Ok(status.op_mode)
    }

    /// Enables 1:1 mode (bypass), for instance to charge from 5 V.
    ///
    /// The mode itself is allowed only in the bypass window: `EN_1TO1` feeds the input
    /// straight to the battery, so with `Vin >= 8 V` (and below 4.2 V) the call is
    /// rejected with [`PumpError::BypassNeedsFiveVoltVin`] - the check is inside, so
    /// no caller can enable 1:1 at a raised input.
    ///
    /// # Errors
    ///
    /// * [`PumpError::BypassNeedsFiveVoltVin`] - Vin outside the bypass window.
    /// * [`PumpError::ModeNotReached`] - the chip did not confirm the mode.
    /// * [`PumpError::Bus`] / [`PumpError::NotOpen`] - bus or session.
    pub fn enable_bypass(&mut self) -> Result<OpMode, PumpError> {
        let vin = self.read_adc(AdcChannel::Vin)?;
        let vbat = u32::try_from(self.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
        if !bypass_allowed_by_vin(vin, vbat) {
            return Err(PumpError::BypassNeedsFiveVoltVin { vin_uv: vin });
        }
        self.set_op_mode(OpMode::Bypass)?;
        let status = self.settle_and_read_status()?;
        // Check the same way as for 2:1: a silent refusal of the chip must not be
        // taken for success, or the driver would think the fallback mode is on
        // while charging is in fact not happening.
        if status.op_mode != OpMode::Bypass {
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Bypass.code(),
                raw_status: status.sys_sts,
            });
        }
        // 1:1 is not 2:1: the session state must be named honestly.
        self.state = PumpState::Configured;
        Ok(status.op_mode)
    }

    /// Enables 2:1 mode and, on failure, a safe bypass.
    ///
    /// Returns the mode actually reached: `Switching` if the chip confirmed the fast
    /// mode, or `Bypass` if it had to fall back. That way the driver is not left
    /// without a working mode because of a single chip or bus failure.
    ///
    /// If neither is confirmed, the error of the first attempt is returned (it
    /// refers to the mode itself); the caller must then put the chip into `standby`,
    /// or it stays in an undefined state.
    ///
    /// # Errors
    ///
    /// * [`PumpError::ModeNotReached`] - the chip confirmed neither 2:1 nor bypass.
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::Bus`] - bus failure.
    pub fn enable_switching_or_bypass(&mut self) -> Result<OpMode, PumpError> {
        match self.enable_switching() {
            Ok(mode) => Ok(mode),
            Err(switching_error) => match self.enable_bypass() {
                Ok(mode) => Ok(mode),
                Err(_) => Err(switching_error),
            },
        }
    }

    /// Re-reads the status, giving the chip time to apply the mode.
    ///
    /// The reference waits 10 ms after the mode write (`msleep(10)`) and only then
    /// reads `SYS_STS`. An empty wait loop is not available to the core, so we
    /// re-read the status several times: one bus transfer takes 8-46 ms, which is
    /// well above the reference's pause.
    ///
    /// # Errors
    ///
    /// Propagates bus errors.
    fn settle_and_read_status(&mut self) -> Result<Status, PumpError> {
        let mut last = self.status()?;
        for _ in 0..3 {
            last = self.status()?;
            if last.op_mode != OpMode::Unknown && last.op_mode != OpMode::Standby {
                break;
            }
        }
        Ok(last)
    }

    /// Explicit charge start/stop, mirroring Android `psy_chg_set_charging_enable`
    /// plus Vin/Vbat-aware mode selection (`cp_qc30` style).
    ///
    /// Order: disable RCP → near-float soft OV/float/taper → clear latched faults
    /// → pick mode from Vin **and Vbat** → request mode → settle/read back.
    /// 2:1 needs `Vin >= 2*Vbat + 250 mV` (and `>= 8 V`); it is never replaced by
    /// 1:1 bypass at elevated Vin — that would put 8 V+ across the battery.
    /// When Vin is elevated but short of `2*Vbat + 250 mV`, no mode is requested
    /// (standby; the caller retries under its own cooldown, not every tick).
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the pump is not open.
    /// * [`PumpError::Bus`] - transfer failure.
    /// * [`PumpError::ModeNotReached`] - chip refused the Vin-appropriate mode.
    ///
    /// `post_reset_delay` is mandatory: the 5 V recovery path does `soft_reset`,
    /// after which POR forbids any I²C transfer until [`regs::SOFT_RESET_DELAY_MS`].
    /// Host tests pass an empty closure, KMDF passes `KeDelayExecutionThread`; the
    /// delay must not be forgotten, because without the parameter the function
    /// is not called.
    pub fn set_charging(
        &mut self,
        on: bool,
        post_reset_delay: &mut dyn FnMut(),
    ) -> Result<OpMode, PumpError> {
        // Step of the reference: before the charge starts, reverse protection is off.
        self.update(regs::SYS_CTRL, 1 << 2, 0, "disable_reverse_current")?;
        if !on {
            // Charging is off - the node counts as open: the POR budget of the input
            // is reset, so the next plug-in has the right to a reset.
            self.por_vin_uv = None;
            self.set_op_mode(OpMode::Standby)?;
            let status = self.settle_and_read_status()?;
            self.op_mode = status.op_mode;
            self.state = PumpState::Configured;
            return Ok(status.op_mode);
        }

        let vin = self.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vbat = u32::try_from(self.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
        let Some(want) = charge_mode(vin, vbat) else {
            let _ = self.set_op_mode(OpMode::Standby);
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Standby.code(),
                raw_status: 0,
            });
        };

        // Watchdog status bit also blocks mode until cleared; force WDT off when
        // the profile disabled it (registry WatchdogEnabled=0).
        if !self.config.watchdog_enabled {
            let _ = self.update(regs::TIMER_CTRL, 1 << 7, 0, "watchdog_force_off");
        }

        self.bypass_stage = 0;

        // Near-float / FAULT1_VBAT_OV: Android cp_qc30 tapers and hands off to
        // SMB; on Windows we still need mode 3. Soft-raise V_FLOAT slightly,
        // mask VBAT_OV (like VIN_OV for QC), clear latch, taper IIN. Never use
        // 1:1 bypass at elevated Vin even if the battery is near full.
        //
        // For the 5 V fallback neither the taper nor clearing the latch is done
        // before the mode request: the sequence verified on 17.09 consisted of
        // clearing the reverse protection (`SYS_CTRL` bit 2) and writing the mode.
        // Everything else is stages 2-3 (`recover_5v_bypass`), and they are applied
        // only after the chip has refused on the clean path.
        if want != OpMode::Bypass {
            self.prepare_near_float_for_charge(true);

            // Live nabu: FAULT1 VIN_OV latches at QC ~12 V and blocks mode change
            // (volt_qual). Clear latch; mask VIN_OV for the elevated 2:1 path only.
            let _ = self.clear_latched_faults();
            if want == OpMode::Switching {
                let _ = self.update(
                    regs::FAULT_CTRL,
                    regs::FAULT_CTRL_DISABLE_VIN_OV,
                    regs::FAULT_CTRL_DISABLE_VIN_OV,
                    "disable_vin_ov_qc",
                );
            }
        }

        let result = match want {
            OpMode::Switching => self.enable_switching(),
            OpMode::Bypass => {
                self.bypass_stage = 1;
                self.enable_bypass()
                    .or_else(|_| self.recover_5v_bypass(post_reset_delay))
            }
            OpMode::Standby | OpMode::Unknown => Err(PumpError::ModeNotReached {
                wanted: want.code(),
                raw_status: 0,
            }),
        };

        match result {
            Ok(mode) => {
                self.op_mode = mode;
                self.state = match mode {
                    OpMode::Switching => PumpState::Switching,
                    _ => PumpState::Configured,
                };
                // Taper and `VBAT_OV` mitigation come after the confirmed mode:
                // they no longer affect the mode request, and they protect the battery
                // at the top of the charge. The through mode does not raise `V_FLOAT`.
                if mode == OpMode::Bypass {
                    self.prepare_near_float_for_charge(false);
                }
                Ok(mode)
            }
            Err(err) => {
                // Do not force standby after a partial 5 V bypass arm — that
                // undoes SYS_CTRL=0x01 before the chip settles (live TA200).
                if want != OpMode::Bypass {
                    let _ = self.set_op_mode(OpMode::Standby);
                }
                Err(err)
            }
        }
    }

    /// Mask UV/OV that latch `FAULT1=0x21` on saggy 5 V bricks (TA200).
    fn arm_5v_bypass_fault_mask(&mut self) -> Result<(), PumpError> {
        self.update(
            regs::FAULT_CTRL,
            regs::FAULT_CTRL_MASK_5V_BYPASS,
            regs::FAULT_CTRL_MASK_5V_BYPASS,
            "mask_5v_bypass_faults",
        )?;
        self.clear_latched_faults()
    }

    /// Soft-reset recovery for the 5 V bypass, stage 2, then the masked
    /// stage 3 if the chip still refuses.
    ///
    /// Stage 2 is the sequence that worked on 17.09 both from the raw tool and
    /// from the driver: `soft_reset` → **caller delay** → `configure` (profile
    /// only) → `SYS_CTRL=0x01`. Stage 3 adds the `.627` overlay (mask UV/OV,
    /// pulse the latch) and is the last resort — nothing in the vendor sources
    /// asks for it. Never used at elevated Vin (would drop the QC latch).
    ///
    /// `post_reset_delay` must sleep ≥ [`regs::SOFT_RESET_DELAY_MS`] before any
    /// further I²C (POR). It comes from the caller of [`Self::set_charging`]:
    /// host tests pass a no-op, KMDF sleeps (`KeDelayExecutionThread`); without a
    /// real delay `configure()` right after `soft_reset` hangs the chip.
    fn recover_5v_bypass(
        &mut self,
        post_reset_delay: &mut dyn FnMut(),
    ) -> Result<OpMode, PumpError> {
        let vin_now =
            u32::try_from(self.read_adc(AdcChannel::Vin).unwrap_or(0).max(0)).unwrap_or(0);
        // POR is once per input. The reference clears the latch with a reset and the
        // chip obeys afterwards; repeating the reset every tick is not allowed: it
        // jerks the charge and erases state the chip may have latched legitimately.
        if let Some(prev) = self.por_vin_uv {
            // Exception from the budget: a live VFAULT latch (`FAULT1 = 0x21`).
            // The `TIMER_CTRL` pulse does not clear it - it only cleans FAULT2
            // (live measurement 0x3F → 0x20, FAULT1 untouched), and with it the chip
            // refuses 1:1 at 4.7-5.0 V: measurement 19.09 10:49 on MDY-11-EP - 39.1 mA,
            // mode 1, `ChargeAttemptN` growing, `LastEnableErr = -4`. POR is the
            // only live sequence after which `FAULT1=0x00` and the bypass holds
            // 2.0-2.7 A. Without the latch the budget works as before.
            let fault1 = self.read(regs::FAULT1_STS).unwrap_or(0);
            if vin_now.abs_diff(prev) <= POR_VIN_TOLERANCE_UV
                && fault1 & regs::FAULT1_VFAULTS_MASK == 0
            {
                return Err(PumpError::ModeNotReached {
                    wanted: OpMode::Bypass.code(),
                    raw_status: self.read(regs::SYS_STS).unwrap_or(0),
                });
            }
        }
        self.por_vin_uv = Some(vin_now);

        let _ = self.soft_reset();
        self.bypass_stage = 2;
        post_reset_delay();
        let _ = self.configure();
        let vin = self.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vbat = u32::try_from(self.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
        if charge_mode(vin, vbat) != Some(OpMode::Bypass) {
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Bypass.code(),
                raw_status: 0,
            });
        }
        // Masked write, like the vendor's `ln8000_change_opmode` (mask
        // `STANDBY_EN|EN_1TO1` = 0x09, `.c:697`): an absolute `SYS_CTRL=0x01`
        // zeroed bits 7:4 and 1, which the vendor never touches. On a live board that
        // gave `SYS_STS=0x28` instead of `BYPASS_ENABLED`.
        self.set_op_mode(OpMode::Bypass)?;
        let status = self.settle_and_read_status()?;
        if status.op_mode == OpMode::Bypass {
            self.state = PumpState::Configured;
            self.prepare_near_float_for_charge(false);
            return Ok(status.op_mode);
        }

        // Stage 3: the UV/OV mask and the latch-clear pulse. The `.627` add-on is
        // undocumented by the vendor; not verified on a live board.
        self.bypass_stage = 3;
        let _ = self.arm_5v_bypass_fault_mask();
        self.set_op_mode(OpMode::Bypass)?;
        let status = self.settle_and_read_status()?;
        if status.op_mode == OpMode::Bypass {
            self.state = PumpState::Configured;
            return Ok(status.op_mode);
        }
        // The mask did not help - put `FAULT_CTRL` back as it was: leaving the node
        // with the protections off is worse than a mode failure.
        let _ = self.update(
            regs::FAULT_CTRL,
            regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "unmask_after_failed_bypass",
        );
        Err(PumpError::ModeNotReached {
            wanted: OpMode::Bypass.code(),
            raw_status: status.sys_sts,
        })
    }

    /// Deliberate taper setpoint near the top of the charge, µA.
    ///
    /// `Some(min(config.iin_limit_ua, VBAT_TAPER_IIN_UA))` while Vbat is in the taper
    /// band, `None` otherwise. The condition is the same as in
    /// `prepare_near_float_for_charge`: a latched `VBAT_OV` and rejection of the
    /// `VBAT ≈ Vin/2` artifact - here it is defined **once**, so that the taper
    /// and the guard do not drift apart in its interpretation.
    ///
    /// The guard needs it: its return to the profile (`guard::evaluate`) must
    /// stop at this setpoint, otherwise it cancels a deliberate current reduction
    /// in the window where the taper and return bands overlap.
    #[must_use]
    pub fn taper_setpoint_ua(&self, vbat_uv: u32, vin_uv: u32, ov_latched: bool) -> Option<u32> {
        if !vbat_near_float_with_vin(vbat_uv, self.config.vbat_float_uv, ov_latched, vin_uv) {
            return None;
        }
        Some(self.config.iin_limit_ua.min(VBAT_TAPER_IIN_UA))
    }

    /// Soft float / `VBAT_OV` / taper when Vbat is near the Nabu float band.
    ///
    /// Matches live recovery (`V_FLOAT` ~4.50 V + latch clear) and Android
    /// taper-at-`bat_volt_lmt−100`. Does not permanently shrink the profile
    /// `iin_limit_ua` — only writes `IIN_CTRL` for this attempt.
    ///
    /// `allow_float_raise` separates two cases. For 2:1 raising `V_FLOAT` is
    /// needed: without it a latched `VBAT_OV` blocks the mode change. For 1:1 it
    /// is forbidden: the verified run on 17.09 went with `V_FLOAT = 0x7D` (4.35 V),
    /// that is **below** the profile, and raising the setpoint in the through mode
    /// would mean feeding the battery from 5 V to a higher voltage. The current
    /// taper does not depend on it and works in both cases.
    fn prepare_near_float_for_charge(&mut self, allow_float_raise: bool) {
        let vbat = self.read_adc(AdcChannel::Vbat).unwrap_or(0);
        let vbat_uv = u32::try_from(vbat.max(0)).unwrap_or(0);
        let vin = self.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vin_uv = u32::try_from(vin.max(0)).unwrap_or(0);
        let fault1 = self.read(regs::FAULT1_STS).unwrap_or(0);
        let ov_latched = fault1 & regs::FAULT1_VBAT_OV != 0;
        let float_uv = self.config.vbat_float_uv;
        // Reject Vin/2 rail artifact (live: 4780 mV @ Vin 9.6 V while pack ~4.47 V).
        let Some(tapered) = self.taper_setpoint_ua(vbat_uv, vin_uv, ov_latched) else {
            return;
        };

        // Soft OV mask + float headroom only when OV is latched or Vbat is at
        // the float ceiling (not merely in the early taper band).
        let at_ceiling = ov_latched || vbat_uv.saturating_add(50_000) >= float_uv;
        if at_ceiling && allow_float_raise {
            let want_float = soft_float_for_vbat(float_uv, vbat_uv);
            if want_float > float_uv {
                // Write float without permanently raising the configured profile
                // beyond the soft max — keep session headroom for retries.
                let code = encode_vbat_float(want_float);
                let _ = self.write_verified(regs::V_FLOAT_CTRL, code, "vbat_float_soft");
            }
            let _ = self.update(
                regs::FAULT_CTRL,
                regs::FAULT_CTRL_DISABLE_VBAT_OV,
                regs::FAULT_CTRL_DISABLE_VBAT_OV,
                "disable_vbat_ov_near_float",
            );
            let _ = self.clear_latched_faults();
        }

        if tapered < self.config.iin_limit_ua {
            // The taper goes through the same write path as `set_iin_limit`, but it
            // does not touch the profile (`config.iin_limit_ua`): this is a
            // "single-shot" setpoint, and `configure()` must return the profile
            // limit. [`Self::applied_iin_ua`] reads what stands in the register: the
            // guard decides the fold-back from it, and the return ceiling comes from
            // [`Self::taper_setpoint_ua`] - otherwise "reduction" raises 1.2 A to 2 A.
            let _ = self.write_iin_limit(tapered, "iin_taper_near_float");
        }
    }

    /// Pulse `TIMER_CTRL` bit 2 to clear latched fault/status (Android).
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] / [`PumpError::Bus`]
    pub fn clear_latched_faults(&mut self) -> Result<(), PumpError> {
        self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_CLEAR_LATCH,
            regs::TIMER_CTRL_CLEAR_LATCH,
            "latch_clear_set",
        )?;
        self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_CLEAR_LATCH,
            0,
            "latch_clear_clr",
        )
    }

    /// Puts the device into standby.
    ///
    /// # Errors
    ///
    /// Propagates bus errors.
    pub fn standby(&mut self) -> Result<(), PumpError> {
        self.set_op_mode(OpMode::Standby)
    }

    /// Services ("feeds") the chip's watchdog timer.
    ///
    /// The watchdog is a protection, not a nuisance: if the driver stops
    /// responding, the chip itself stops the charge after the chosen period
    /// (5/10/20/40 s). So whoever enabled the watchdog via
    /// `PumpConfig::watchdog_enabled` must call this function more often than the
    ///
    /// period - the driver does it from the telemetry timer. Period bits are
    /// If the watchdog is off, the call is safe and does not enable it.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the chip is not open.
    /// * [`PumpError::Bus`] - bus failure.
    pub fn service_watchdog(&mut self) -> Result<(), PumpError> {
        if !self.config.watchdog_enabled {
            return Ok(());
        }
        self.update(regs::TIMER_CTRL, 1 << 7, 1 << 7, "watchdog_service")
    }

    /// Updates the input current limit.
    ///
    /// # Errors
    ///
    /// * [`PumpError::OutOfRange`] - current below [`regs::IIN_MIN_UA`].
    /// * [`PumpError::Bus`] - bus failure.
    pub fn set_iin_limit(&mut self, iin_ua: u32) -> Result<u8, PumpError> {
        let code = self.write_iin_limit(iin_ua, "iin_limit")?;
        self.config.iin_limit_ua = iin_ua;
        Ok(code)
    }

    /// Writes the input current setpoint into `IIN_CTRL` without touching the profile.
    ///
    /// The single write point: both [`Self::set_iin_limit`] and the taper near the
    /// top of the charge set the setpoint this way. The register read back is the
    /// source of truth about what actually stands in the chip (see [`Self::applied_iin_ua`]).
    ///
    /// # Errors
    ///
    /// * [`PumpError::OutOfRange`] - current below [`regs::IIN_MIN_UA`].
    /// * [`PumpError::Bus`] - bus failure.
    fn write_iin_limit(&mut self, iin_ua: u32, field: &'static str) -> Result<u8, PumpError> {
        let code = encode_iin_limit(iin_ua)?;
        self.update(regs::IIN_CTRL, 0x7F, code, field)?;
        Ok(code)
    }

    /// The input current setpoint actually written, µA.
    ///
    /// It is read from the register, not from the profile: `config.iin_limit_ua`
    /// is "how much was ordered", and the taper near the top of the charge writes
    /// past it (1.2 A at the 2.8 A profile). The fold-back decision must rest on
    /// what stands in the chip, otherwise "reduce to 2 A" raises 1.2 A.
    ///
    /// `None` - the register was not read: the setpoint is unknown, so the current cannot decide.
    #[must_use]
    pub fn applied_iin_ua(&mut self) -> Option<u32> {
        self.read_register(regs::IIN_CTRL)
            .ok()
            .map(decode_iin_limit)
    }

    /// Updates the target charge voltage.
    ///
    /// # Errors
    ///
    /// [`PumpError::Bus`] - bus failure.
    pub fn set_vbat_float(&mut self, vbat_uv: u32) -> Result<u8, PumpError> {
        let code = encode_vbat_float(vbat_uv);
        self.write_verified(regs::V_FLOAT_CTRL, code, "vbat_float")?;
        self.config.vbat_float_uv = vbat_uv;
        Ok(code)
    }

    /// Reads a status snapshot.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::Bus`] - bus failure.
    pub fn status(&mut self) -> Result<Status, PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        let sys_sts = self.read(regs::SYS_STS)?;
        let safety_sts = self.read(regs::SAFETY_STS)?;
        let fault1_sts = self.read(regs::FAULT1_STS)?;
        let fault2_sts = self.read(regs::FAULT2_STS)?;
        let ldo_sts = self.read(regs::LDO_STS)?;
        let op_mode = OpMode::from_sys_sts(sys_sts);
        self.op_mode = op_mode;
        Ok(Status {
            sys_sts,
            op_mode,
            safety_sts,
            fault1_sts,
            fault2_sts,
            ldo_sts,
        })
    }

    /// Reads one ADC channel sample.
    ///
    /// The code occupies two neighbouring registers (10 bits), so it is read as a pair.
    ///
    /// # Errors
    ///
    /// [`PumpError::Bus`] - bus failure.
    /// Reads an ADC channel value.
    ///
    /// While the two sample bytes are read, ADC updates stop and then resume -
    /// otherwise the bytes could come from different conversions, and the
    /// temperature would be garbage. The reference driver does the same
    /// (`ln8000_get_adc_data`): sets the pause bit, reads the pair, clears the bit.
    ///
    /// The pause is cleared on a read error too: otherwise the ADC would stay stopped.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the chip is not open.
    /// * [`PumpError::Bus`] - the bus did not respond.
    pub fn read_adc(&mut self, channel: AdcChannel) -> Result<i32, PumpError> {
        self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_PAUSE_ADC,
            regs::TIMER_CTRL_PAUSE_ADC,
            "adc_pause",
        )?;
        let outcome = self.read_pair(channel.register());
        // Clear the pause in any case, and only then parse the result.
        let _ = self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_PAUSE_ADC,
            0,
            "adc_resume",
        );
        Ok(channel.decode(outcome?))
    }

    /// Performs a software reset of the device.
    ///
    /// After the reset the device returns to the "default" state, so the session
    /// is marked [`PumpState::Probed`] again - the configuration must be applied
    /// anew. The [`regs::SOFT_RESET_DELAY_MS`] pause is observed by the calling
    /// side.
    ///
    /// # Errors
    ///
    /// [`PumpError::Bus`] - bus failure.
    pub fn soft_reset(&mut self) -> Result<(), PumpError> {
        self.write(regs::LION_CTRL, regs::LION_CTRL_UNLOCK)?;
        // Absolute write, no verify/readback: the soft-reset bit self-clears and
        // the chip PORs. Live nabu: `update`+`verify_writes` hung the I²C
        // controller mid-read after BC_OP_2 bit0 (IOCTL WRITE_REG / SET_CHARGE
        // paths). Caller must wait [`regs::SOFT_RESET_DELAY_MS`] then configure.
        let current = self.read(regs::BC_OP_2).unwrap_or(0);
        self.write(regs::BC_OP_2, current | (1 << 0))?;
        self.state = PumpState::Probed;
        self.op_mode = OpMode::Unknown;
        Ok(())
    }

    /// Reads a register directly (diagnostics).
    ///
    /// Used by the driver's service interface when a register needs to be seen
    /// for which there is no dedicated method.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::Bus`] - bus failure.
    pub fn read_register(&mut self, addr: u8) -> Result<u8, PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        self.read(addr)
    }

    /// Writes a register directly (diagnostics), verified by reading back.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - the session is closed.
    /// * [`PumpError::OutOfRange`] - the value read back did not match the written one.
    pub fn write_register(&mut self, addr: u8, value: u8) -> Result<(), PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        // Soft-reset via diagnostic WR must not verify (same hang as soft_reset).
        if addr == regs::BC_OP_2 && value & (1 << 0) != 0 {
            return self.write(addr, value);
        }
        self.write_verified(addr, value, "diagnostic")
    }

    /// Closes the session: the device is put into standby.
    ///
    /// Errors on close are swallowed: driver unload must not depend on bus
    /// availability.
    pub fn close(&mut self) {
        if self.state == PumpState::Closed {
            return;
        }
        // Masked, like `ln8000_change_opmode` (mask 0x09): an absolute
        // `STANDBY_EN` write zeroed bits 7:4 and 1 of `SYS_CTRL`.
        let _ = self.update(
            regs::SYS_CTRL,
            OpMode::sys_ctrl_mask(),
            regs::SYS_CTRL_STANDBY_EN,
            "close_standby",
        );
        self.state = PumpState::Closed;
        self.op_mode = OpMode::Standby;
    }

    // --- internal ---

    fn set_op_mode(&mut self, target: OpMode) -> Result<(), PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        let bits = target.sys_ctrl_bits()?;
        self.update(regs::SYS_CTRL, OpMode::sys_ctrl_mask(), bits, "op_mode")?;
        self.op_mode = target;
        Ok(())
    }

    fn read(&mut self, addr: u8) -> Result<u8, PumpError> {
        let mut attempt: u8 = 0;
        loop {
            match self.bus.read(addr) {
                Ok(value) => {
                    self.reads = self.reads.saturating_add(1);
                    return Ok(value);
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !bus_recoverable(&err) || attempt > self.config.max_bus_retries {
                        self.state = PumpState::Faulted;
                        return Err(PumpError::Bus(err));
                    }
                    let _ = self.bus.reset();
                }
            }
        }
    }

    fn read_pair(&mut self, addr: u8) -> Result<u16, PumpError> {
        let mut attempt: u8 = 0;
        loop {
            match self.bus.read_pair(addr) {
                Ok(value) => {
                    self.reads = self.reads.saturating_add(2);
                    return Ok(value);
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !bus_recoverable(&err) || attempt > self.config.max_bus_retries {
                        self.state = PumpState::Faulted;
                        return Err(PumpError::Bus(err));
                    }
                    let _ = self.bus.reset();
                }
            }
        }
    }

    fn write(&mut self, addr: u8, value: u8) -> Result<(), PumpError> {
        let mut attempt: u8 = 0;
        loop {
            match self.bus.write(addr, value) {
                Ok(()) => {
                    self.writes = self.writes.saturating_add(1);
                    return Ok(());
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !bus_recoverable(&err) || attempt > self.config.max_bus_retries {
                        self.state = PumpState::Faulted;
                        return Err(PumpError::Bus(err));
                    }
                    let _ = self.bus.reset();
                }
            }
        }
    }

    fn update(
        &mut self,
        addr: u8,
        mask: u8,
        value: u8,
        field: &'static str,
    ) -> Result<(), PumpError> {
        let current = self.read(addr)?;
        let updated = (current & !mask) | (value & mask);
        self.write(addr, updated)?;
        if self.config.verify_writes {
            let read_back = self.read(addr)?;
            if read_back != updated {
                return Err(self.fault_out_of_range(field, u32::from(updated)));
            }
        }
        Ok(())
    }

    fn write_verified(
        &mut self,
        addr: u8,
        value: u8,
        field: &'static str,
    ) -> Result<(), PumpError> {
        self.write(addr, value)?;
        if self.config.verify_writes {
            let read_back = self.read(addr)?;
            if read_back != value {
                return Err(self.fault_out_of_range(field, u32::from(value)));
            }
        }
        Ok(())
    }

    fn fault_out_of_range(&mut self, field: &'static str, requested: u32) -> PumpError {
        self.state = PumpState::Faulted;
        PumpError::OutOfRange { field, requested }
    }
}

impl<T: RegisterBus> Drop for Pump<T> {
    /// Puts the device back into standby; errors are swallowed.
    fn drop(&mut self) {
        self.close();
    }
}

fn bus_recoverable(err: &BusError) -> bool {
    matches!(
        err.kind,
        crate::error::BusErrorKind::Timeout
            | crate::error::BusErrorKind::Disconnected
            | crate::error::BusErrorKind::Io
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::{GuardAction, GuardLimits, evaluate};
    use crate::session::TelemetrySample;
    use crate::testkit::{Fault, MockPumpBus};

    #[test]
    fn open_detects_wrong_chip() {
        let mut bus = MockPumpBus::new();
        bus.set_reg(regs::DEVICE_ID, 0x11);
        let err = Pump::open(bus, PumpConfig::default()).unwrap_err();
        assert!(matches!(err, PumpError::WrongDeviceId { got: 0x11 }));
    }

    #[test]
    fn open_reports_dead_bus() {
        let mut bus = MockPumpBus::new();
        bus.push_fault(Fault::ReadError {
            addr: regs::DEVICE_ID,
            times: 5,
        });
        let config = PumpConfig {
            max_bus_retries: 1,
            ..PumpConfig::default()
        };
        let err = Pump::open(bus, config).unwrap_err();
        assert_eq!(err.code(), "bus");
    }

    #[test]
    fn configure_writes_expected_registers() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        assert_eq!(pump.state(), PumpState::Configured);

        // Check the key values the driver must write.
        assert_eq!(
            pump.bus.reg(regs::V_FLOAT_CTRL),
            encode_vbat_float(NABU_VBAT_FLOAT_UV)
        );
        assert_eq!(pump.bus.reg(regs::IIN_CTRL) & 0x7F, 40); // 2 A / 50 mA
        assert_eq!(
            pump.bus.reg(regs::THRESHOLD_CTRL),
            regs::THRESHOLD_CTRL_DEFAULT
        );
        assert_eq!(pump.bus.reg(regs::ADC_CFG), 0x3E);
        assert_eq!(pump.bus.reg(regs::CHARGE_CTRL) & (1 << 7), 1 << 7);
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_STANDBY_EN,
            1 << 3
        );
        // Regulation loops are enabled ("disable" bits cleared, "int" bits set).
        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(regulation & (1 << 5), 0);
        assert_eq!(regulation & (1 << 4), 0);
        assert_eq!(regulation & (1 << 7), 1 << 7);
        assert_eq!(regulation & (1 << 6), 1 << 6);
    }

    #[test]
    fn enable_switching_reaches_mode_three() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        assert_eq!(pump.enable_switching().unwrap(), OpMode::Switching);
        assert_eq!(pump.state(), PumpState::Switching);
        assert_eq!(pump.status().unwrap().op_mode, OpMode::Switching);
    }

    #[test]
    fn switching_is_reported_when_chip_refuses() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // The device ignores the command and stays in standby.
        pump.bus.push_fault(Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY,
        });
        let err = pump.enable_switching().unwrap_err();
        assert!(matches!(err, PumpError::ModeNotReached { .. }));
        assert!(err.is_recoverable());
    }

    #[test]
    fn bypass_mode_sets_1to1_bit() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        let mode = pump.enable_bypass().unwrap();
        assert_eq!(mode, OpMode::Bypass);
        assert_eq!(pump.bus.reg(regs::SYS_CTRL) & 1, 1);
    }

    #[test]
    fn iin_limit_updates_and_rejects_low_values() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        let code = pump.set_iin_limit(3_000_000).unwrap();
        assert_eq!(code, 60);
        assert_eq!(pump.bus.reg(regs::IIN_CTRL) & 0x7F, 60);
        let err = pump.set_iin_limit(10_000).unwrap_err();
        assert_eq!(err.code(), "out_of_range");
    }

    #[test]
    fn status_reports_faults() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.bus.set_reg(regs::FAULT1_STS, regs::FAULT1_WATCHDOG);
        let status = pump.status().unwrap();
        assert!(status.watchdog_expired());
        assert!(status.has_critical_fault());
        assert_eq!(status.fault_summary(), "watchdog");
    }

    #[test]
    fn adc_channels_are_read() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.bus.set_reg(AdcChannel::Vbat.register(), 0x2B);
        pump.bus.set_reg(AdcChannel::Vbat.register() + 1, 0x01);
        pump.bus.set_reg(AdcChannel::Iin.register(), 0xC8);
        // Code 0x012B = 299, step 5 mV: 299 × 5 mV = 1.495 V (no offset).
        assert_eq!(pump.read_adc(AdcChannel::Vbat).unwrap(), 1_495_000);
        // Code 200 → 200 × 4.89 mA = 978 mA
        assert_eq!(pump.read_adc(AdcChannel::Iin).unwrap(), 978_000);
    }

    #[test]
    fn adc_read_pauses_and_resumes_conversion_update() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        // A foreign bit in TIMER_CTRL must survive the read-modify-write.
        pump.bus.set_reg(regs::TIMER_CTRL, 0b1000_0000);
        pump.bus.set_reg(AdcChannel::Vbat.register(), 0x2B);
        pump.bus.set_reg(AdcChannel::Vbat.register() + 1, 0x01);

        assert_eq!(pump.read_adc(AdcChannel::Vbat).unwrap(), 1_495_000);

        let timer = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(
            timer & regs::TIMER_CTRL_PAUSE_ADC,
            0,
            "the ADC update pause must be cleared after the read"
        );
        assert_eq!(
            timer & 0b1000_0000,
            0b1000_0000,
            "the foreign bit in TIMER_CTRL must be preserved"
        );
    }

    #[test]
    fn switching_or_bypass_prefers_fast_mode() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        assert_eq!(
            pump.enable_switching_or_bypass().unwrap(),
            OpMode::Switching,
            "if the chip confirmed 2:1, we stay in it"
        );
    }

    #[test]
    fn switching_or_bypass_falls_back_only_on_the_five_volt_side() {
        // 5 V: 2:1 is not confirmed, but 1:1 is allowed - the mode still engages.
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        // The chip "does not hear" the 2:1 command and always reports bypass.
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_BYPASS_ENABLED,
        });
        assert_eq!(
            pump.enable_switching_or_bypass().unwrap(),
            OpMode::Bypass,
            "at 5 V, when 2:1 fails, bypass must engage"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            regs::SYS_CTRL_EN_1TO1,
            "the 1:1 bit must be set in SYS_CTRL"
        );

        // Elevated Vin: falling back to 1:1 is forbidden (that is 9 V on the battery),
        // the error of the original mode is returned, the 1:1 bit is not set.
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_000_000);
        set_vbat_uv(&mut pump, 4_275_000);
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_BYPASS_ENABLED,
        });
        let err = pump.enable_switching_or_bypass().unwrap_err();
        assert!(matches!(err, PumpError::ModeNotReached { .. }), "{err:?}");
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 at 9 V - overvoltage on the battery"
        );
    }

    #[test]
    fn switching_or_bypass_reports_error_when_nothing_confirmed() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY,
        });
        let err = pump.enable_switching_or_bypass().unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "mode error expected, got: {err:?}"
        );
        // After such a refusal the driver puts the chip into standby: check that it works.
        assert!(pump.standby().is_ok(), "standby must be confirmed");
    }

    /// Pack Vin ADC registers so `read_adc(Vin)` returns approximately `uv`.
    fn set_vin_uv(pump: &mut Pump<MockPumpBus>, uv: i32) {
        let units = u16::try_from((uv / 16_000).clamp(0, 1023)).unwrap_or(0);
        let high = u8::try_from((units / 16) & 0x3F).unwrap_or(0);
        let low = u8::try_from((units % 16) * 16).unwrap_or(0);
        let register = AdcChannel::Vin.register();
        pump.bus_mut().set_reg(register, low);
        pump.bus_mut().set_reg(register + 1, high);
    }

    fn set_vbat_uv(pump: &mut Pump<MockPumpBus>, uv: i32) {
        let units = u16::try_from((uv / 5_000).clamp(0, 1023)).unwrap_or(0);
        let high = u8::try_from((units / 256) & 0x03).unwrap_or(0);
        let low = u8::try_from(units % 256).unwrap_or(0);
        let register = AdcChannel::Vbat.register();
        pump.bus_mut().set_reg(register, low);
        pump.bus_mut().set_reg(register + 1, high);
    }

    #[test]
    fn set_charging_uses_bypass_at_five_volts() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        assert_eq!(pump.set_charging(true, &mut || {}).unwrap(), OpMode::Bypass);
        // The clean 1:1 path does not touch `FAULT_CTRL`: the mask is a stage 3
        // add-on, and its absence is exactly what distinguished the working 17.09
        // run from `.627`-`.628`.
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "stage 1 has no right to mask faults"
        );
    }

    #[test]
    fn set_charging_5v_soft_resets_when_bypass_blocked() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.set_reg(regs::FAULT1_STS, 0x21);
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let mut por_delays = 0_u32;
        assert_eq!(
            pump.set_charging(true, &mut || por_delays += 1).unwrap(),
            OpMode::Bypass
        );
        assert_eq!(
            por_delays, 1,
            "5 V recovery must survive POR after soft_reset"
        );
        assert_eq!(
            pump.bypass_stage(),
            2,
            "the mode was confirmed by the POR stage, not by the mask"
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "the profile POR path does not mask faults"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            regs::SYS_CTRL_EN_1TO1
        );
    }

    #[test]
    fn set_charging_5v_reports_stage_three_when_nothing_works() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.push_fault(crate::testkit::Fault::RefuseSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "the error must name the mode, not the bus: {err:?}"
        );
        assert_eq!(
            pump.bypass_stage(),
            3,
            "the last attempt was the fault mask"
        );
        // The mask did not help - it must not stay on the node: protections
        // left off after a failed attempt are worse than the mode failure itself.
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "a failed stage 3 must restore FAULT_CTRL as it was"
        );
    }

    #[test]
    fn set_charging_5v_does_not_reset_twice_on_one_input() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.push_fault(crate::testkit::Fault::RefuseSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let mut por_delays = 0_u32;
        assert!(pump.set_charging(true, &mut || por_delays += 1).is_err());
        assert_eq!(por_delays, 1, "the first attempt has the right to POR");
        assert!(pump.por_spent());

        // Same input: a second POR is forbidden - otherwise the driver jerks the
        // charge every telemetry tick and erases legitimately latched state.
        let _ = pump.set_charging(true, &mut || por_delays += 1);
        assert_eq!(por_delays, 1, "a repeated POR on the same Vin is forbidden");

        // The input changed within the 5 V window (another adapter): the POR budget
        // opens anew. The 5 V → 9 V change does not affect the budget - there 1:1
        // is not requested at all.
        set_vin_uv(&mut pump, 5_500_000);
        let _ = pump.set_charging(true, &mut || por_delays += 1);
        assert_eq!(por_delays, 2, "a new input is a new POR budget");
    }

    #[test]
    fn charging_off_opens_a_fresh_por_budget() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.push_fault(crate::testkit::Fault::RefuseSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let mut por_delays = 0_u32;
        assert!(pump.set_charging(true, &mut || por_delays += 1).is_err());
        assert!(pump.por_spent());

        let _ = pump.set_charging(false, &mut || por_delays += 1);
        assert!(
            !pump.por_spent(),
            "disabling the charge opens the node: the POR budget is reset"
        );
    }

    #[test]
    fn set_charging_five_volt_path_does_not_wait_without_soft_reset() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        let mut por_delays = 0_u32;
        assert_eq!(
            pump.set_charging(true, &mut || por_delays += 1).unwrap(),
            OpMode::Bypass
        );
        assert_eq!(por_delays, 0, "no POR pause is needed without soft_reset");
    }

    #[test]
    fn enable_bypass_refuses_elevated_vin() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // F1/F2: the single gate inside enable_bypass - 9 V on the battery is unacceptable.
        set_vin_uv(&mut pump, 9_000_000);
        set_vbat_uv(&mut pump, 4_275_000);
        let err = pump.enable_bypass().unwrap_err();
        assert!(
            matches!(
                err,
                PumpError::BypassNeedsFiveVoltVin { vin_uv }
                    if vin_uv >= crate::encoding::SWITCHING_MIN_VIN_UV
            ),
            "{err:?}"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 bit must stay clear at 9 V"
        );
        // The same at the 2:1 boundary and at 12 V.
        for vin in [8_000_000, 12_000_000] {
            set_vin_uv(&mut pump, vin);
            assert!(matches!(
                pump.enable_bypass().unwrap_err(),
                PumpError::BypassNeedsFiveVoltVin { .. }
            ));
        }
        // In the bypass window the mode still engages.
        set_vin_uv(&mut pump, 5_000_000);
        assert_eq!(pump.enable_bypass().unwrap(), OpMode::Bypass);
    }

    #[test]
    fn set_charging_uses_switching_at_nine_volts() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_000_000);
        assert_eq!(
            pump.set_charging(true, &mut || {}).unwrap(),
            OpMode::Switching
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_DISABLE_VIN_OV,
            regs::FAULT_CTRL_DISABLE_VIN_OV,
            "elevated Vin must mask VIN_OV"
        );
    }

    #[test]
    fn set_charging_elevated_vin_without_headroom_stays_in_standby() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // Live nabu case: the PD brick gives 8.416 V at a 4.275 V battery.
        // 2:1 needs >= 2*4.275 + 0.25 = 8.8 V and physically cannot pull it.
        set_vin_uv(&mut pump, 8_416_000);
        set_vbat_uv(&mut pump, 4_275_000);
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "no voltage headroom - no mode requested: {err:?}"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 at elevated Vin would put 8+ V on the battery"
        );
        assert_eq!(
            pump.status().unwrap().op_mode,
            OpMode::Standby,
            "a failed attempt must leave the chip in standby"
        );
    }

    #[test]
    fn set_charging_never_picks_bypass_at_eight_volts() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // 8.0 V at a charged battery (4.4 V): 2:1 does not pass, bypass is forbidden.
        set_vin_uv(&mut pump, 8_000_000);
        set_vbat_uv(&mut pump, 4_400_000);
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(matches!(err, PumpError::ModeNotReached { .. }), "{err:?}");
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "the upper bypass bound is SWITCHING_MIN_VIN_UV"
        );
        // Below 8 V bypass is allowed again.
        set_vin_uv(&mut pump, 5_000_000);
        assert_eq!(pump.set_charging(true, &mut || {}).unwrap(), OpMode::Bypass);
    }

    #[test]
    fn set_charging_elevated_vin_does_not_fallback_to_bypass() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 12_000_000);
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY,
        });
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "must not silently bypass at 12 V: {err:?}"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 bit must stay clear after failed elevated start"
        );
    }

    #[test]
    fn set_charging_near_float_soft_clears_vbat_ov() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_600_000);
        set_vbat_uv(&mut pump, 4_520_000);
        pump.bus.set_reg(regs::FAULT1_STS, regs::FAULT1_VBAT_OV);
        assert_eq!(
            pump.set_charging(true, &mut || {}).unwrap(),
            OpMode::Switching
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_DISABLE_VBAT_OV,
            regs::FAULT_CTRL_DISABLE_VBAT_OV,
            "near-float must soft-mask VBAT_OV"
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_DISABLE_VIN_OV,
            regs::FAULT_CTRL_DISABLE_VIN_OV,
            "elevated Vin must still mask VIN_OV"
        );
        let float_code = pump.bus.reg(regs::V_FLOAT_CTRL);
        assert!(
            float_code >= encode_vbat_float(crate::encoding::VBAT_FLOAT_SOFT_MAX_UV),
            "soft float should reach ~4.50 V headroom, got 0x{float_code:02X}"
        );
        let iin = pump.bus.reg(regs::IIN_CTRL) & 0x7F;
        assert!(
            iin <= encode_iin_limit(crate::encoding::VBAT_TAPER_IIN_UA).unwrap(),
            "near-float must taper IIN"
        );
    }

    #[test]
    fn near_float_taper_setpoint_is_the_one_the_guard_sees() {
        // F10: the taper near the top of the charge writes 1.2 A past the profile
        // (2.8 A stays in `config`). The guard must see the setpoint from `IIN_CTRL`:
        // by the profile it would "reduce" the current to the 2.0 A band, that is
        // raise it from 1.2 A at 44 °C and 4.46 V.
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_600_000);
        set_vbat_uv(&mut pump, 4_460_000);
        assert_eq!(
            pump.set_charging(true, &mut || {}).unwrap(),
            OpMode::Switching
        );

        let applied = pump.applied_iin_ua().expect("IIN_CTRL reads back");
        assert_eq!(
            applied, VBAT_TAPER_IIN_UA,
            "the taper should have written 1.2 A into the chip"
        );
        assert_eq!(
            pump.config().iin_limit_ua,
            PumpConfig::for_qc35_class_b().iin_limit_ua,
            "the taper does not touch the profile - `config` still says 2.8 A"
        );
        // The helper is the single source of truth about the deliberate setpoint: it must
        // return exactly what the taper wrote into the register, and stay silent outside.
        assert_eq!(
            pump.taper_setpoint_ua(4_460_000, 9_600_000, false),
            Some(VBAT_TAPER_IIN_UA),
            "the deliberate setpoint in the taper band is 1.2 A"
        );
        assert_eq!(
            pump.taper_setpoint_ua(4_300_000, 9_600_000, false),
            None,
            "outside the taper band there is no deliberate setpoint"
        );
        assert_eq!(
            pump.taper_setpoint_ua(4_800_000, 9_600_000, false),
            None,
            "the VBAT ≈ Vin/2 artifact does not count as a taper"
        );

        let mut limits = GuardLimits::standard();
        limits.iin_profile_ua = pump.config().iin_limit_ua;
        limits.vbat_reduce_uv = crate::encoding::NABU_VBAT_NON_FFC_UV;
        // Both fold-back conditions at once: the temperature is above the fold-back
        // threshold and 4.46 V >= 4.45 V. The threshold comes from the profile: it
        // must lie above this board's crystal idle temperature (live 19.09: 46.1 °C at idle).
        let sample = TelemetrySample {
            ts_ms: 1_000,
            vbat_uv: 4_460_000,
            vbus_uv: 9_600_000,
            iin_ua: 1_200_000,
            die_temp_dc: limits.temp_reduce_dc + 1,
            op_mode: OpMode::Switching,
            input_present: true,
            vbat_valid: true,
            die_temp_valid: true,
        };
        assert_eq!(
            evaluate(
                &sample,
                &limits,
                pump.applied_iin_ua(),
                pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, false)
            ),
            GuardAction::None,
            "1.2 A is below the fold-back band: the guard has no right to raise the current"
        );
        // The same sample, but with the profile setpoint (2.8 A) - a normal fold-back.
        assert_eq!(
            evaluate(&sample, &limits, Some(2_800_000), None),
            GuardAction::ReduceCurrent {
                to_ua: 2_000_000,
                reason: "die_temp_reduce",
            }
        );
    }

    #[test]
    fn clear_latched_faults_pulses_timer_bit() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.bus.set_reg(regs::TIMER_CTRL, 0xB0);
        pump.clear_latched_faults().unwrap();
        assert_eq!(
            pump.bus.reg(regs::TIMER_CTRL) & regs::TIMER_CTRL_CLEAR_LATCH,
            0
        );
        assert_eq!(pump.bus.reg(regs::TIMER_CTRL) & 0xB0, 0xB0);
    }

    #[test]
    fn watchdog_service_keeps_timer_bits() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(
            bus,
            PumpConfig {
                watchdog_enabled: true,
                watchdog_period: WatchdogPeriod::Sec10,
                ..PumpConfig::default()
            },
        )
        .unwrap();
        pump.configure().unwrap();

        let after_configure = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(
            after_configure & (1 << 7),
            1 << 7,
            "the watchdog must be enabled"
        );
        assert_eq!(
            after_configure & (0b11 << 5),
            WatchdogPeriod::Sec10.code() << 5,
            "the period must be written into bits 5-6"
        );

        // The chip clears the bit itself after it fires; servicing restores it
        // and does not touch the period.
        pump.bus.set_reg(regs::TIMER_CTRL, 0);
        pump.service_watchdog().unwrap();
        let after_service = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(
            after_service & (1 << 7),
            1 << 7,
            "servicing enables the watchdog"
        );
        assert_eq!(
            after_service & (0b11 << 5),
            0,
            "servicing does not write foreign bits"
        );
    }

    #[test]
    fn watchdog_service_is_noop_when_disabled() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        let before = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(before & (1 << 7), 0, "the watchdog is off by default");
        pump.service_watchdog().unwrap();
        assert_eq!(
            pump.bus.reg(regs::TIMER_CTRL),
            before,
            "a disabled watchdog must not enable itself"
        );
    }

    #[test]
    fn registry_parameters_change_profile_and_reject_junk() {
        let mut config = PumpConfig::for_qc35_class_b();

        assert!(config.apply_parameter("IinLimitUa", 1_500_000));
        assert_eq!(config.iin_limit_ua, 1_500_000);
        assert!(config.apply_parameter("VbatFloatUv", 4_400_000));
        assert_eq!(config.vbat_float_uv, 4_400_000);
        assert!(config.apply_parameter("VacOvpUv", 11_000_000));
        assert_eq!(config.vac_ovp_uv, 11_000_000);
        assert!(config.apply_parameter("NtcAlarmCfg", 226));
        assert_eq!(config.ntc_alarm_cfg, 226);
        assert!(config.apply_parameter("WatchdogEnabled", 1));
        assert!(config.watchdog_enabled);

        // Out-of-range values and unknown names are rejected and spoil nothing.
        let before = config;
        assert!(!config.apply_parameter("IinLimitUa", 10));
        assert!(!config.apply_parameter("VbatFloatUv", 9_000_000));
        assert!(!config.apply_parameter("VacOvpUv", 3_000_000));
        assert!(!config.apply_parameter("NtcAlarmCfg", 0x0400));
        assert!(!config.apply_parameter("TotallyDifferentParameter", 1));
        assert_eq!(config, before, "invalid values must not change the profile");
    }

    #[test]
    fn protection_profile_parameter_switches_protections() {
        let mut config = PumpConfig::for_nabu_dts();
        assert!(
            config.tdie_prot_disabled,
            "the base is the tablet configuration"
        );

        assert!(config.apply_parameter("ProtectionProfile", 1));
        assert!(
            !config.tdie_prot_disabled,
            "profile 1 enables the protections"
        );
        assert!(!config.iin_ocp_disabled);

        assert!(config.apply_parameter("ProtectionProfile", 0));
        assert!(
            config.tdie_prot_disabled,
            "profile 0 returns the DTS configuration"
        );

        assert!(
            !config.apply_parameter("ProtectionProfile", 7),
            "an unknown profile is rejected"
        );
    }

    #[test]
    fn qc35_profile_enables_vfloat_loop_for_windows() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).unwrap();
        pump.configure().unwrap();
        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(
            regulation & (1 << 5),
            0,
            "Windows PEIC must keep VFLOAT regulation enabled"
        );
        assert_eq!(
            regulation & (1 << 4),
            0,
            "Windows PEIC must keep IIN regulation enabled"
        );
        assert_ne!(regulation & (1 << 7), 0, "vfloat loop int enabled");
        assert_ne!(regulation & (1 << 6), 0, "iin loop int enabled");
    }

    #[test]
    fn nabu_dts_profile_disables_pump_protections() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::for_nabu_dts()).unwrap();
        pump.configure().unwrap();

        // Regulation and temperature loops are off - the tablet DTS requires it.
        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(regulation & (1 << 7), 0, "the vfloat loop is off");
        assert_eq!(regulation & (1 << 6), 0, "the iin loop is off");
        assert_eq!(
            regulation & (1 << 5),
            1 << 5,
            "vfloat regulation is disabled"
        );
        assert_eq!(regulation & (1 << 4), 1 << 4, "iin regulation is disabled");
        assert_eq!(regulation & (1 << 2), 0, "die protection is disabled");
        assert_eq!(regulation & (1 << 1), 0, "die regulation is disabled");
        assert_eq!(
            regulation & (0b11 << 2),
            regs::NTC_SHUTDOWN_CFG << 2,
            "the NTC configuration is intact"
        );

        // The hardware voltage protections stay enabled.
        let fault = pump.bus.reg(regs::FAULT_CTRL);
        assert_eq!(fault & (1 << 6), 1 << 6, "iin ocp is disabled per DTS");
        assert_eq!(fault & (1 << 5), 0, "vbat ovp is enabled");
        assert_eq!(fault & (1 << 4), 0, "vac ov is enabled");

        // The bus and battery temperature monitors are off per DTS too.
        let recovery = pump.bus.reg(regs::RECOVERY_CTRL);
        assert_eq!(recovery & 0b11, 0, "the bus and battery monitors are off");
    }

    #[test]
    fn protective_profile_enables_pump_protections() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::protective()).unwrap();
        pump.configure().unwrap();

        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(regulation & (1 << 7), 1 << 7, "the vfloat loop is on");
        assert_eq!(regulation & (1 << 6), 1 << 6, "the iin loop is on");
        assert_eq!(regulation & (1 << 5), 0, "vfloat regulation is enabled");
        assert_eq!(regulation & (1 << 4), 0, "iin regulation is enabled");
        assert_eq!(regulation & (1 << 2), 1 << 2, "die protection is enabled");
        assert_eq!(regulation & (1 << 1), 1 << 1, "die regulation is enabled");

        let fault = pump.bus.reg(regs::FAULT_CTRL);
        // IIN_OCP is off in any profile: the tablet's vendor DT does not keep
        // it, and the `FAULT2_IIN_OC` latch parks the pump in standby.
        assert_eq!(fault & (1 << 6), 1 << 6, "iin ocp is off");
        let recovery = pump.bus.reg(regs::RECOVERY_CTRL);
        assert_eq!(recovery & 0b11, 0b11, "the temperature monitors are on");
    }

    #[test]
    fn soft_reset_returns_to_probed_state() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.enable_switching().unwrap();
        pump.soft_reset().unwrap();
        assert_eq!(pump.state(), PumpState::Probed);
        assert_eq!(pump.bus.reg(regs::LION_CTRL), regs::LION_CTRL_UNLOCK);
        assert_eq!(pump.bus.reg(regs::BC_OP_2) & 1, 1);
        // The configuration must be applied anew.
        pump.configure().unwrap();
        assert_eq!(pump.enable_switching().unwrap(), OpMode::Switching);
    }

    #[test]
    fn diagnostic_register_access_round_trips() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.write_register(regs::GLITCH_CTRL, 0x0C).unwrap();
        assert_eq!(pump.read_register(regs::GLITCH_CTRL).unwrap(), 0x0C);
        pump.close();
        assert_eq!(
            pump.read_register(regs::GLITCH_CTRL).unwrap_err().code(),
            "not_open"
        );
    }

    #[test]
    fn close_and_drop_put_device_to_standby() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.enable_switching().unwrap();
        pump.close();
        assert_eq!(pump.state(), PumpState::Closed);
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_STANDBY_EN,
            1 << 3
        );
        assert_eq!(pump.status().unwrap_err().code(), "not_open");
    }

    #[test]
    fn drop_closes_session() {
        let bus = MockPumpBus::new();
        {
            let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
            pump.configure().unwrap();
            pump.enable_switching().unwrap();
        }
        // After Drop the bus must receive the standby command.
        let mut probe = MockPumpBus::new();
        probe.set_reg(regs::SYS_CTRL, 0);
        assert!(Pump::open(probe, PumpConfig::default()).is_ok());
    }

    #[test]
    fn verify_failure_is_detected() {
        let bus = MockPumpBus::new();
        let config = PumpConfig {
            verify_writes: true,
            ..PumpConfig::default()
        };
        let mut pump = Pump::open(bus, config).unwrap();
        pump.bus.push_fault(Fault::WrongReadBack {
            addr: regs::V_FLOAT_CTRL,
            value: 0x00,
            times: 1,
        });
        let err = pump.configure().unwrap_err();
        assert_eq!(err.code(), "out_of_range");
        assert_eq!(pump.state(), PumpState::Faulted);
    }
}
