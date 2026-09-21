//! LN8000 charge pump driver core for the Xiaomi Pad 5 (`nabu`).
//!
//! The LN8000 is the second charging stage: a 2:1 converter that allows the
//! adapter to supply 9 V and deliver double the current into the battery. On
//! Android it is serviced by the `ln8000_charger.c` driver; under Windows there
//! is no driver, so this crate repeats that driver's logic in portable form.
//!
//! # What the core does
//!
//! 1. Checks that the device on the bus really is an LN8000 ([`Pump::open`]).
//! 2. Configures thresholds and protections ([`Pump::configure`]) — as `ln8000_init_device()`.
//! 3. Enables 2:1 mode and checks that the chip accepted it ([`Pump::enable_switching`]).
//! 4. Reads status and faults ([`Pump::status`]), samples the ADC
//!    ([`Pump::read_adc`]).
//! 5. Can do a software reset ([`Pump::soft_reset`]) and switch to standby
//!    ([`Pump::standby`], [`Pump::close`], [`Drop`]).
//!
//! # Responsibility boundaries
//!
//! * The crate knows nothing about the I²C controller: the transport is behind the
//!   [`RegisterBus`] trait.
//! * The crate does not sleep: the caller performs the post-reset delay and
//!   the watchdog timer servicing.
//! * The crate does not negotiate voltage with the adapter: that is the job of
//!
//! # Example
//!
//! ```
//! use ln8000::testkit::MockPumpBus;
//! use ln8000::{AdcChannel, OpMode, Pump, PumpConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut pump = Pump::open(MockPumpBus::new(), PumpConfig::for_qc35_class_b())?;
//! pump.configure()?;
//! assert_eq!(pump.enable_switching()?, OpMode::Switching);
//!
//! let status = pump.status()?;
//! assert!(!status.has_critical_fault());
//! assert_eq!(status.op_mode, OpMode::Switching);
//!
//! let vbat = pump.read_adc(AdcChannel::Vbat)?;
//! println!("battery voltage: {vbat} µV");
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::arithmetic_side_effects))]

pub mod battery_policy;
pub mod driver;
pub mod encoding;
pub mod error;
pub mod guard;
pub mod hvdcp_policy;
pub mod qc35_auth;
pub mod regs;
pub mod session;
pub mod status;
pub mod transport;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use driver::{Pump, PumpConfig, PumpState};
pub use encoding::{
    AdcHibernateDelay, AdcMode, CHARGE_MIN_VIN_UV, NABU_BAT_OVP_UV, NABU_QC3_BAT_VOLT_MAX_UV,
    NABU_VBAT_FLOAT_UV, NABU_VBAT_NON_FFC_UV, OpMode, SWITCHING_HEADROOM_UV, SWITCHING_MIN_VIN_UV,
    VBAT_FLOAT_SOFT_MAX_UV, VBAT_TAPER_IIN_UA, VBAT_TAPER_MARGIN_UV, WatchdogPeriod,
    bypass_allowed_by_vin, charge_mode, decode_iin_limit, decode_vbat_float, encode_iin_limit,
    encode_ntc_alarm, encode_vac_ovp, encode_vbat_float, min_vin_for_switching_uv,
    soft_float_for_vbat, vbat_near_float, vbat_near_float_with_vin, vbat_tracks_converter_rail,
};
pub use error::{BusError, BusErrorKind, PumpError};
pub use guard::{
    BYPASS_DENIED_STRIKES_BEFORE_STOP, BypassResolution, DIE_TEMP_MAX_PLAUSIBLE_DC, GuardAction,
    GuardLimits, TEMP_REDUCE_HYST_DC, VBAT_REDUCE_HYST_UV, bypass_strikes_expired, die_temp_usable,
    evaluate, resolve_bypass,
};
pub use hvdcp_policy::{
    ApsdElevate, FORCE9V_EXTEND_MS, FORCE9V_HARD_CAP_MS, FORCE9V_RISE_UV, FORCE9V_SETTLE_MS,
    Force9vWait, HVDCP_ERR_USBIN_UNAVAILABLE, HVDCP_PHASE_DONE, HVDCP_PHASE_FAILED,
    HVDCP_PHASE_FIVE_V_BYPASS, HVDCP_PHASE_IDLE, HVDCP_SUPERUSER_RETRY_MAX,
    HVDCP_SUPERUSER_RETRY_MS, VIN_UNPLUG_MAX_UV, apsd_elevate_path, force9v_extended_deadline,
    force9v_step, input_present_from_vin, promote_qc_charger, should_renegotiate_on_input_edge,
    should_schedule_superuser_retry, superuser_retry_due,
};
pub use qc35_auth::{
    ICL_RAW_QC35_2A, ICL_RAW_QC35_40W, QC35_18W_HI_UV, QC35_27W_HI_UV, QC35_27W_LO_UV,
    QC35_40W_LO_UV, QC35_AUTH_PULSE_GAP_MS, QC35_CAP_HI_UV, QC35_CAP_LO_UV, QC35_CAP_TIMEOUT_MS,
    QC35_CONFIRM_PULSES, QC35_DETECT_HI_UV, QC35_DETECT_LO_UV, QC35_DETECT_TIMEOUT_MS,
    QC35_PREP_MAX_INC, QC35_SRC_CAP_PULSES, QC35_STEP_UV, QC35_VIN_POLL_MS, Qc35AuthGate,
    Qc35AuthPulse, Qc35AuthResult, decide_qc35_auth_attempt, qc35_authenticate_outcome,
    qc35_cap_window, qc35_detect_window, qc35_icl_raw, qc35_power_limit_w,
};
pub use regs::RegAddr;
pub use session::{ChargeSession, Telemetry, TelemetrySample};
pub use status::{AdcChannel, Status};
pub use transport::RegisterBus;
