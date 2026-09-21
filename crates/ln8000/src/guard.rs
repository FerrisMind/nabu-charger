//! Temperature and current protection: deciding what to do with the charging mode.
//!
//! The driver must not only enable the fast mode but also limit it in time. The
//! module takes the latest telemetry sample and the limits, and returns an action:
//! do nothing, reduce the current, fall back to bypass, or stop the charge
//! completely.
//!
//! Thresholds use the same units as the telemetry: temperature in tenths of
//! °C, current in microamps, voltage in microvolts.

use crate::encoding::{bypass_allowed_by_vin, vbat_tracks_converter_rail};
use crate::session::TelemetrySample;

/// Current restore hysteresis on battery voltage, µV.
///
/// Until Vbat falls below `vbat_reduce_uv − VBAT_REDUCE_HYST_UV`, the setpoint
/// holds: the 50 mV band keeps the guard from "chattering" between fold-back and
/// restore at the threshold boundary.
pub const VBAT_REDUCE_HYST_UV: u32 = 50_000;

/// Current restore hysteresis on die temperature, tenths of °C (3.0 °C).
pub const TEMP_REDUCE_HYST_DC: i32 = 30;

/// Upper bound of a plausible die temperature, tenths of °C.
///
/// The ADC scale is clipped at +160.0 °C, and the **zero raw code** lands there
/// too (channel failure: `AdcChannel::DieTemp.decode(0) == 1600`). Anything above
/// 125.0 °C counts as invalid: a working die never heats up that far, and the
/// safety threshold [`GuardLimits::temp_stop_dc`] (55.0 °C) sits well below the
/// boundary, so an honest sample behaves as before.
pub const DIE_TEMP_MAX_PLAUSIBLE_DC: i32 = 1_250;

/// Protection limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardLimits {
    /// Temperature from which current is reduced (tenths of °C).
    pub temp_reduce_dc: i32,
    /// Temperature at which we fall back to bypass (tenths of °C).
    pub temp_bypass_dc: i32,
    /// Temperature at which charging stops (tenths of °C).
    pub temp_stop_dc: i32,
    /// Maximum input current, µA.
    pub iin_max_ua: u32,
    /// Target input current limit, µA.
    pub iin_target_ua: u32,
    /// Minimum input current in bypass, µA.
    pub iin_floor_ua: u32,
    /// Profile input current limit of the pump, µA.
    ///
    /// Snapshot of `PumpConfig::iin_limit_ua` at configuration time: the guard
    /// returns current to it when voltage and temperature leave the fold-back
    /// band. `config.iin_limit_ua` itself will not do: `Pump::set_iin_limit`
    /// overwrites it with every setpoint, and the "profile" value is lost. By
    /// default it equals [`Self::iin_target_ua`]: if no snapshot was taken, the
    /// restore will not raise the current above the target.
    pub iin_profile_ua: u32,
    /// Battery voltage at which current is reduced (µV).
    ///
    /// This is the **full charge** threshold, not the QC3 loop: for NABU the
    /// non-FFC target is 4.45 V (`qcom,non-fcc-fv-max-uv`), FFC is 4.47 V
    /// (`qcom,fv-max-uv`, dtbo-03). 4.42 V (`mi,qc3-bat-volt-max`) is the QC3 loop
    /// limit; current must not be cut on it, or charging stalls long before full.
    pub vbat_reduce_uv: u32,
}

impl Default for GuardLimits {
    fn default() -> Self {
        Self::standard()
    }
}

impl GuardLimits {
    /// Default profile, available in a const context.
    ///
    /// The thresholds are raised above this board's **live idle temperature**. The
    /// old 43.0/48.0/55.0 °C sat below it: the nabu die at idle (pump in standby,
    /// input at the ADC floor) holds 43.5-46.1 °C, measured 19.09 via `die_dc`,
    /// so the temperature fold-back fired constantly, and restore required
    /// ≤ 40.0 °C (threshold minus the 3.0 °C hysteresis), which the die never
    /// reaches. Under 2:1 load it goes up to 50.8 °C, and then `FallbackToBypass`
    /// fired, forbidden at 9 V: the current fell to [`Self::iin_floor_ua`] and
    /// stayed there forever - a live `IIN_CTRL = 10` (500 mA) against a 2.7 A
    /// profile.
    ///
    /// The vendor keeps `tdie-prot-disable` and `tdie-reg-disable` in the DTS for
    /// nabu (`nabu-sm8150.dtsi`): the die hardware loops are off, SMB runs CV. The
    /// software fold-back must sit **above** the working temperature, otherwise it
    /// is not protection but a permanent charge brake.
    ///
    /// 55.0/60.0/65.0 °C - above the measured load (50.8 °C) and well below the
    /// hardware scale maximum (+160 °C).
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            temp_reduce_dc: 550,
            temp_bypass_dc: 600,
            temp_stop_dc: 650,
            iin_max_ua: 3_500_000,
            iin_target_ua: 2_000_000,
            iin_floor_ua: 500_000,
            iin_profile_ua: 2_000_000,
            // FFC charge threshold of NABU (4.47 V = `qcom,fv-max-uv`): current is
            // cut only at the very top, not from 4.42 V (the QC3 loop limit).
            vbat_reduce_uv: 4_470_000,
        }
    }

    /// Strict profile: charging without the fast mode.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            temp_reduce_dc: 400,
            temp_bypass_dc: 430,
            temp_stop_dc: 500,
            iin_max_ua: 1_500_000,
            iin_target_ua: 1_000_000,
            iin_floor_ua: 500_000,
            iin_profile_ua: 1_000_000,
            vbat_reduce_uv: 4_350_000,
        }
    }

    /// Checks that the thresholds are set in a sensible order.
    ///
    /// The order is essential: current reduction must come before the fallback to
    /// bypass, and bypass before the stop. A violated order means a guard that
    /// either fires late or cuts the charge at once. The same for currents:
    /// floor ≤ target ≤ maximum.
    #[must_use]
    pub const fn is_consistent(&self) -> bool {
        self.temp_reduce_dc < self.temp_bypass_dc
            && self.temp_bypass_dc < self.temp_stop_dc
            && self.iin_floor_ua <= self.iin_target_ua
            && self.iin_target_ua <= self.iin_max_ua
    }

    /// Fold-back band setpoint: `min(profile limit, target)`, µA.
    ///
    /// While Vbat is at the top of the charge or the die is hot, the guard keeps
    /// **one and the same** setpoint instead of subtracting step by step: repeated
    /// calls do not "creep" down to the floor in a few ticks.
    #[must_use]
    pub const fn band_iin_ua(&self) -> u32 {
        if self.iin_profile_ua < self.iin_target_ua {
            self.iin_profile_ua
        } else {
            self.iin_target_ua
        }
    }

    /// Profile limit with an eye on the protection bounds, µA.
    ///
    /// Current returns here once both voltage and temperature have left the
    /// fold-back band below the hysteresis thresholds.
    #[must_use]
    pub const fn restore_iin_ua(&self) -> u32 {
        if self.iin_profile_ua > self.iin_max_ua {
            self.iin_max_ua
        } else if self.iin_profile_ua < self.iin_floor_ua {
            self.iin_floor_ua
        } else {
            self.iin_profile_ua
        }
    }

    /// Applies a parameter from the registry to the protection thresholds.
    ///
    /// A value out of range **or violating the threshold order** is rejected
    /// entirely: the set stays as it was instead of becoming partially updated.
    #[must_use]
    pub fn apply_parameter(&mut self, name: &str, value: u32) -> bool {
        let mut candidate = *self;
        match name {
            "TempReduceDc" => match i32::try_from(value) {
                Ok(temp) if (200..=600).contains(&temp) => candidate.temp_reduce_dc = temp,
                _ => return false,
            },
            "TempBypassDc" => match i32::try_from(value) {
                Ok(temp) if (200..=650).contains(&temp) => candidate.temp_bypass_dc = temp,
                _ => return false,
            },
            "TempStopDc" => match i32::try_from(value) {
                Ok(temp) if (250..=700).contains(&temp) => candidate.temp_stop_dc = temp,
                _ => return false,
            },
            "IinMaxUa" => {
                if !(100_000..=6_850_000).contains(&value) {
                    return false;
                }
                candidate.iin_max_ua = value;
            }
            "IinTargetUa" => {
                if !(100_000..=6_850_000).contains(&value) {
                    return false;
                }
                candidate.iin_target_ua = value;
            }
            "IinFloorUa" => {
                if !(100_000..=6_850_000).contains(&value) {
                    return false;
                }
                candidate.iin_floor_ua = value;
            }
            "VbatReduceUv" => {
                if !(3_000_000..=4_500_000).contains(&value) {
                    return false;
                }
                candidate.vbat_reduce_uv = value;
            }
            _ => return false,
        }
        if !candidate.is_consistent() {
            return false;
        }
        *self = candidate;
        true
    }

    /// Applies a single value **without** checking the threshold order.
    ///
    /// Needed where the thresholds arrive as a set: [`Self::apply_parameter`]
    /// checks consistency after every value, so a set that changes several
    /// thresholds at once is never applied - the very first value is compared
    /// against neighbours that have not been updated yet. Live case
    /// 19.09: `TempReduceDc=600`, `TempBypassDc=650`, `TempStopDc=700` - 600
    /// is rejected against the old `TempBypassDc=480`, and so it is in any order.
    ///
    /// The value range is checked here too (garbage does not pass), while the
    /// order is checked once in [`Self::validate`] after the whole set. Until
    /// `validate` passes, the set counts as not applied: the caller must put
    /// the snapshot back.
    #[must_use]
    pub fn apply_parameter_lenient(&mut self, name: &str, value: u32) -> bool {
        match name {
            "TempReduceDc" => match i32::try_from(value) {
                Ok(temp) if (200..=600).contains(&temp) => self.temp_reduce_dc = temp,
                _ => return false,
            },
            "TempBypassDc" => match i32::try_from(value) {
                Ok(temp) if (200..=650).contains(&temp) => self.temp_bypass_dc = temp,
                _ => return false,
            },
            "TempStopDc" => match i32::try_from(value) {
                Ok(temp) if (250..=700).contains(&temp) => self.temp_stop_dc = temp,
                _ => return false,
            },
            "IinMaxUa" => {
                if !(100_000..=6_850_000).contains(&value) {
                    return false;
                }
                self.iin_max_ua = value;
            }
            "IinTargetUa" => {
                if !(100_000..=6_850_000).contains(&value) {
                    return false;
                }
                self.iin_target_ua = value;
            }
            "IinFloorUa" => {
                if !(100_000..=6_850_000).contains(&value) {
                    return false;
                }
                self.iin_floor_ua = value;
            }
            "VbatReduceUv" => {
                if !(3_000_000..=4_500_000).contains(&value) {
                    return false;
                }
                self.vbat_reduce_uv = value;
            }
            _ => return false,
        }
        true
    }

    /// Whether the threshold set is consistent (see [`Self::is_consistent`]).
    ///
    /// Separated from [`Self::apply_parameter`] for sets: there it is called on
    /// every value, here once for the whole set.
    #[must_use]
    pub const fn validate(&self) -> bool {
        self.is_consistent()
    }
}

/// What to do with the charging mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuardAction {
    /// Change nothing.
    None,
    /// Reduce the input current limit to the given value.
    ReduceCurrent {
        /// New limit, µA.
        to_ua: u32,
        /// Reason for the journal.
        reason: &'static str,
    },
    /// Return the input current limit to the profile value.
    ///
    /// Appears only after both voltage and temperature have left the fold-back
    /// band below the hysteresis thresholds: without such an explicit restore the
    /// fold-back would stay forever.
    RestoreCurrent {
        /// Profile limit, µA.
        to_ua: u32,
        /// Reason for the journal.
        reason: &'static str,
    },
    /// Fall back to 1:1 bypass mode (slow but safe charging).
    FallbackToBypass {
        /// Reason for the journal.
        reason: &'static str,
    },
    /// Stop charging completely (standby).
    Stop {
        /// Reason for the journal.
        reason: &'static str,
    },
}

impl GuardAction {
    /// Whether the action requires a write to the device.
    #[must_use]
    pub const fn is_change(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Short action name for the journal.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReduceCurrent { .. } => "reduce_current",
            Self::RestoreCurrent { .. } => "restore_current",
            Self::FallbackToBypass { .. } => "fallback_bypass",
            Self::Stop { .. } => "stop",
        }
    }
}

/// Whether the die temperature is fit for a protection decision.
///
/// The channel must have been read ([`TelemetrySample::die_temp_valid`]), and the
/// value must be plausible ([`DIE_TEMP_MAX_PLAUSIBLE_DC`]). A failed read gives a
/// zero raw code, that is +160.0 °C at the decoder output - such a sample may be
/// used neither to stop charging nor to fall back to 1:1.
#[must_use]
pub const fn die_temp_usable(sample: &TelemetrySample) -> bool {
    sample.die_temp_valid && sample.die_temp_dc <= DIE_TEMP_MAX_PLAUSIBLE_DC
}

/// Whether the overtemperature episode that resets the 1:1 denial counter has ended.
///
/// The sign is a valid temperature that fell below [`GuardLimits::temp_bypass_dc`]
/// by [`TEMP_REDUCE_HYST_DC`]. Without the reset, a second episode ≥ `temp_bypass_dc`
/// would stop charging on its first tick, skipping the current reduction step:
/// `denied_strikes` is kept between episodes. An invalid sample does not count as
/// a reset - the counter holds until an honest measurement.
#[must_use]
pub const fn bypass_strikes_expired(sample: &TelemetrySample, limits: &GuardLimits) -> bool {
    die_temp_usable(sample)
        && sample.die_temp_dc <= limits.temp_bypass_dc.saturating_sub(TEMP_REDUCE_HYST_DC)
}

/// Computes the guard action from the latest sample and the input current setpoint.
///
/// `applied_iin_ua` is the setpoint **actually in the chip** (the driver reads
/// `IIN_CTRL`): the guard returns an action only if the setpoint really has to
/// change. The profile will not do here - the taper at the top of the charge
/// writes 1.2 A past the profile (`config.iin_limit_ua` stays 2.8 A), and a
/// decision "by the profile" would raise current instead of reducing it. `None`
/// means the register was not read: such a tick is left without current decisions
/// rather than substituting the profile. This also removes the "chatter": while
/// the fold-back condition holds, the setpoint is one and the same
/// ([`GuardLimits::band_iin_ua`]). Current returns to the profile limit via
/// [`GuardAction::RestoreCurrent`] after voltage and temperature drop below hysteresis.
///
/// `deliberate_iin_ua` is the **deliberate** setpoint of the owner that is currently
/// driving current outside the guard bands ([`crate::Pump::taper_setpoint_ua`] at the
/// top of the charge). The restore undoes only the guard's own reduction: it raises
/// the limit no higher than that setpoint and never lowers it. Without it (`None`)
/// the behaviour is as before - back to the profile. Without such a ceiling the
/// guard and the taper fight over one register: in the window where the bands
/// overlap, a restore every 250 ms would cancel the deliberate 1.2 A.
///
/// An invalid sample (the VBAT channel not read, no temperature channel or an
/// implausible one) produces no reduce or restore decisions: a true zero and a
/// read failure are indistinguishable by value, and an error towards raising
/// current is more dangerous than a missed fold-back. Only the thresholds on a
/// valid temperature remain - `temp_stop_dc` and `temp_bypass_dc`.
#[must_use]
pub fn evaluate(
    sample: &TelemetrySample,
    limits: &GuardLimits,
    applied_iin_ua: Option<u32>,
    deliberate_iin_ua: Option<u32>,
) -> GuardAction {
    if !sample.input_present {
        return GuardAction::None;
    }

    // The "stop" and "1:1" thresholds do not depend on the current setpoint, but
    // they need a valid temperature: a channel failure must not stop the charge.
    let temp_ok = die_temp_usable(sample);
    if temp_ok {
        if sample.die_temp_dc >= limits.temp_stop_dc {
            return GuardAction::Stop {
                reason: "die_temp_stop",
            };
        }
        if sample.die_temp_dc >= limits.temp_bypass_dc {
            return GuardAction::FallbackToBypass {
                reason: "die_temp_bypass",
            };
        }
    }

    // Below are the current decisions: they need both the setpoint from the chip
    // and the valid channels they are taken from (die temperature and battery voltage).
    let Some(applied) = applied_iin_ua else {
        return GuardAction::None;
    };
    if !temp_ok || !sample.vbat_valid {
        return GuardAction::None;
    }

    if sample.die_temp_dc >= limits.temp_reduce_dc {
        return cap(applied, limits.band_iin_ua(), "die_temp_reduce");
    }

    if sample.iin_ua > limits.iin_max_ua {
        return cap(applied, limits.iin_target_ua, "iin_over_limit");
    }

    // VBAT ≈ Vin/2 is not the cell but the middle of the switch-cap bus (documented
    // in `encoding::vbat_tracks_converter_rail`): such a sample allows neither
    // cutting nor restoring current, it says nothing about the battery.
    let vbat_credible = !vbat_tracks_converter_rail(sample.vbat_uv, sample.vbus_uv);

    if vbat_credible && sample.vbat_uv >= limits.vbat_reduce_uv {
        return cap(applied, limits.band_iin_ua(), "vbat_reduce");
    }

    // Below is only the restore to the profile, and only when the limit really is
    // too low. The deliberate setpoint (taper at the top of the charge) is the
    // restore ceiling: the guard undoes only **its own** reduction and never
    // lowers the limit itself - lowering is left to the fold-back paths and the taper.
    let restore = match deliberate_iin_ua {
        Some(deliberate) => limits.restore_iin_ua().min(deliberate),
        None => limits.restore_iin_ua(),
    };
    if applied >= restore {
        return GuardAction::None;
    }
    let temp_low = sample.die_temp_dc <= limits.temp_reduce_dc.saturating_sub(TEMP_REDUCE_HYST_DC);
    let vbat_low = !vbat_credible
        || sample.vbat_uv <= limits.vbat_reduce_uv.saturating_sub(VBAT_REDUCE_HYST_UV);
    if temp_low && vbat_low {
        return GuardAction::RestoreCurrent {
            to_ua: restore,
            reason: "reduce_band_exit",
        };
    }

    GuardAction::None
}

/// Fold-back band setpoint: written only if the current limit is higher.
///
/// It does not raise the limit above the setpoint: otherwise the guard would steal
/// the reduction of a stricter action (e.g. 1:1 denied by Vin, limit at the floor).
fn cap(applied_iin_ua: u32, to_ua: u32, reason: &'static str) -> GuardAction {
    if applied_iin_ua <= to_ua {
        GuardAction::None
    } else {
        GuardAction::ReduceCurrent { to_ua, reason }
    }
}

/// How many ticks in a row the guard tolerates overtemperature when 1:1 is denied by Vin.
///
/// `1` means: the first tick with `FallbackToBypass` at a raised Vin reduces current,
/// the second stops the charge. Waiting longer is not allowed: the safe retreat
/// (1:1) is unavailable, and the temperature is above `temp_bypass_dc`.
pub const BYPASS_DENIED_STRIKES_BEFORE_STOP: u32 = 1;

/// What to do with a guard demand to go to 1:1 when Vin does not allow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BypassResolution {
    /// Vin inside the bypass window - 1:1 is allowed.
    Allowed,
    /// 1:1 is denied: reduce the input current to the minimum and wait for the next sample.
    ReduceCurrent {
        /// New limit, µA.
        to_ua: u32,
        /// Reason for the journal.
        reason: &'static str,
    },
    /// 1:1 is denied and the temperature does not fall: stop the charge.
    Stop {
        /// Reason for the journal.
        reason: &'static str,
    },
}

/// Resolves the conflict "guard asks for 1:1, input outside the bypass window".
///
/// `denied_strikes` is how many times in a row this happened before this tick.
/// At a raised Vin the guard reduces current instead of 1:1, and if the temperature
/// persists longer than [`BYPASS_DENIED_STRIKES_BEFORE_STOP`] ticks, it stops the charge.
#[must_use]
pub fn resolve_bypass(
    vin_uv: i32,
    vbat_uv: u32,
    denied_strikes: u32,
    limits: &GuardLimits,
) -> BypassResolution {
    if bypass_allowed_by_vin(vin_uv, vbat_uv) {
        return BypassResolution::Allowed;
    }
    if denied_strikes >= BYPASS_DENIED_STRIKES_BEFORE_STOP {
        BypassResolution::Stop {
            reason: "die_temp_bypass_no_vin_headroom",
        }
    } else {
        BypassResolution::ReduceCurrent {
            to_ua: limits.iin_floor_ua,
            reason: "die_temp_bypass_no_vin_headroom",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::OpMode;

    /// Guard call with the setpoint that is already in the chip: this is how the driver
    /// calls it (`Pump::applied_iin_ua`). A separate wrapper so that the tests do not
    /// bristle with `Some(...)` and check the band logic itself. These scenarios have
    /// no deliberate setpoint (`None`): the taper does not drive them.
    fn evaluate_applied(
        sample: &TelemetrySample,
        limits: &GuardLimits,
        applied_iin_ua: u32,
    ) -> GuardAction {
        evaluate(sample, limits, Some(applied_iin_ua), None)
    }

    /// Profile as in `PumpConfig::for_qc35_class_b`: pump limit 2.8 A.
    ///
    /// The snapshot of the profile limit is taken by the caller (`read_parameters`), so
    /// in tests it has to be set by hand: otherwise the fold-back band would coincide
    /// with the target, and there would be nothing to cut.
    fn profile_limits() -> GuardLimits {
        let mut limits = GuardLimits::standard();
        limits.iin_profile_ua = 2_800_000;
        // The NABU non-FFC target is 4.45 V (`qcom,non-fcc-fv-max-uv`): that is what
        // the device registry holds; FFC 4.47 V is set by an explicit `VbatReduceUv`.
        limits.vbat_reduce_uv = crate::encoding::NABU_VBAT_NON_FFC_UV;
        limits
    }

    fn sample(iin_ua: u32, temp_dc: i32, vbat_uv: u32) -> TelemetrySample {
        TelemetrySample {
            ts_ms: 1_000,
            vbat_uv,
            // Live Vin of fast charging is 9.6 V: the "VBAT ≈ Vin/2" artefact
            // lives at 4.72…4.88 V and does not touch the working charge thresholds.
            vbus_uv: 9_600_000,
            iin_ua,
            die_temp_dc: temp_dc,
            op_mode: OpMode::Switching,
            input_present: true,
            vbat_valid: true,
            die_temp_valid: true,
        }
    }

    #[test]
    fn limits_accept_registry_values() {
        let mut limits = GuardLimits::standard();
        // The fold-back threshold stays below the 1:1 fallback and the stop, so a single
        // value also passes along the strict path.
        assert!(limits.apply_parameter("TempReduceDc", 560));
        assert_eq!(limits.temp_reduce_dc, 560);
        assert!(limits.apply_parameter("TempStopDc", 660));
        assert_eq!(limits.temp_stop_dc, 660);
        assert!(limits.apply_parameter("IinTargetUa", 2_500_000));
        assert_eq!(limits.iin_target_ua, 2_500_000);
        assert!(limits.apply_parameter("IinFloorUa", 1_000_000));
        assert!(limits.apply_parameter("VbatReduceUv", 4_300_000));
        assert!(limits.is_consistent(), "the set must stay consistent");
    }

    #[test]
    fn limits_reject_broken_order_and_junk() {
        let mut limits = GuardLimits::standard();
        let before = limits;

        // Current reduction after the fallback to bypass - the guard would break the order.
        // A 610.0 °C fold-back sits above the bypass (600.0 °C), so it is rejected.
        assert!(!limits.apply_parameter("TempReduceDc", 610));
        // Target below the floor - inconsistent currents.
        assert!(!limits.apply_parameter("IinTargetUa", 100_000));
        // Values outside the ranges and unknown names.
        assert!(!limits.apply_parameter("TempStopDc", 10_000));
        assert!(!limits.apply_parameter("IinMaxUa", 10));
        assert!(!limits.apply_parameter("NoSuchThreshold", 1));

        assert_eq!(
            limits, before,
            "rejected values must not change the threshold set partially"
        );
    }

    #[test]
    fn band_and_restore_limits_stay_inside_the_profile_bounds() {
        // Profile above the protection bound: the restore must not break the maximum.
        let mut limits = GuardLimits::standard();
        limits.iin_profile_ua = 4_000_000;
        assert_eq!(limits.restore_iin_ua(), limits.iin_max_ua);
        assert_eq!(limits.band_iin_ua(), limits.iin_target_ua);
        // Profile below the floor: the restore must not go under the minimum.
        limits.iin_profile_ua = 100_000;
        assert_eq!(limits.restore_iin_ua(), limits.iin_floor_ua);
        assert_eq!(limits.band_iin_ua(), 100_000);
    }

    #[test]
    fn normal_conditions_change_nothing() {
        let limits = GuardLimits::default();
        let action = evaluate_applied(
            &sample(2_000_000, 350, 4_000_000),
            &limits,
            limits.restore_iin_ua(),
        );
        assert_eq!(action, GuardAction::None);
        assert!(!action.is_change());
    }

    #[test]
    fn overheating_caps_current_to_the_band_setpoint() {
        let limits = profile_limits();
        let hot = limits.temp_reduce_dc + 1;
        let action = evaluate_applied(
            &sample(2_800_000, hot, 4_000_000),
            &limits,
            limits.restore_iin_ua(),
        );
        match action {
            GuardAction::ReduceCurrent { to_ua, reason } => {
                assert_eq!(reason, "die_temp_reduce");
                assert_eq!(to_ua, limits.band_iin_ua());
                assert_eq!(to_ua, 2_000_000);
            }
            other => panic!("expected a current limit, got {other:?}"),
        }
        // The applied setpoint is not written again.
        assert_eq!(
            evaluate_applied(&sample(2_800_000, hot, 4_000_000), &limits, 2_000_000),
            GuardAction::None
        );
    }

    #[test]
    fn severe_heat_falls_back_to_bypass() {
        let limits = GuardLimits::default();
        let action = evaluate_applied(
            &sample(2_000_000, limits.temp_bypass_dc, 4_000_000),
            &limits,
            limits.restore_iin_ua(),
        );
        assert_eq!(
            action,
            GuardAction::FallbackToBypass {
                reason: "die_temp_bypass"
            }
        );
        assert_eq!(action.label(), "fallback_bypass");
    }

    #[test]
    fn critical_heat_stops_charging() {
        let limits = GuardLimits::default();
        let action = evaluate_applied(
            &sample(2_000_000, limits.temp_stop_dc, 4_000_000),
            &limits,
            limits.restore_iin_ua(),
        );
        assert_eq!(
            action,
            GuardAction::Stop {
                reason: "die_temp_stop"
            }
        );
    }

    #[test]
    fn overcurrent_is_capped_to_target() {
        let limits = profile_limits();
        let action = evaluate_applied(
            &sample(4_000_000, 350, 4_000_000),
            &limits,
            limits.restore_iin_ua(),
        );
        match action {
            GuardAction::ReduceCurrent { to_ua, reason } => {
                assert_eq!(reason, "iin_over_limit");
                assert_eq!(to_ua, limits.iin_target_ua);
            }
            other => panic!("expected a current limit, got {other:?}"),
        }
    }

    #[test]
    fn vbat_band_setpoint_does_not_creep_and_restores_below_hysteresis() {
        let limits = profile_limits();
        let applied = limits.restore_iin_ua();
        let near_full = sample(2_800_000, 350, 4_460_000);

        let first = evaluate_applied(&near_full, &limits, applied);
        assert_eq!(
            first,
            GuardAction::ReduceCurrent {
                to_ua: 2_000_000,
                reason: "vbat_reduce",
            },
            "at the top of the charge the setpoint is the band, not a step down"
        );
        // Twenty ticks in a row: the setpoint does not "creep" down (in a previous
        // revision it fell to the 500 mA floor within nine ticks and stayed there).
        for tick in 0..20 {
            assert_eq!(
                evaluate_applied(&near_full, &limits, applied),
                first,
                "tick {tick}: the setpoint must be the same"
            );
        }
        // A stricter action (the floor after a denied 1:1) is not cancelled.
        assert_eq!(
            evaluate_applied(&near_full, &limits, limits.iin_floor_ua),
            GuardAction::None
        );
        // 4.44 V is inside the hysteresis band (restore threshold 4.40 V): we hold.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 350, 4_440_000), &limits, 2_000_000),
            GuardAction::None
        );
        // 4.40 V = 4.45 - 0.05: an explicit return to the profile limit.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 350, 4_400_000), &limits, 2_000_000),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );
        // Left the band, but the limit is the profile already - nothing to restore.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 350, 4_000_000), &limits, 2_800_000),
            GuardAction::None
        );
    }

    #[test]
    fn die_temp_band_caps_without_creeping_and_restores_below_hysteresis() {
        let limits = profile_limits();
        // Fold-back threshold is `temp_reduce_dc` (55.0 °C); we take 0.1 °C above it.
        let hot = sample(2_800_000, limits.temp_reduce_dc + 1, 4_000_000);
        let capped = GuardAction::ReduceCurrent {
            to_ua: 2_000_000,
            reason: "die_temp_reduce",
        };
        assert_eq!(evaluate_applied(&hot, &limits, 2_800_000), capped);
        for tick in 0..20 {
            assert_eq!(
                evaluate_applied(&hot, &limits, 2_800_000),
                capped,
                "tick {tick}: the setpoint must be the same"
            );
        }
        // Inside the hysteresis band (restore threshold `temp_reduce_dc − 3.0 °C`): we hold.
        assert_eq!(
            evaluate_applied(
                &sample(2_000_000, limits.temp_reduce_dc - 10, 4_000_000),
                &limits,
                2_000_000
            ),
            GuardAction::None
        );
        // Exactly the restore threshold - an explicit return to the profile limit.
        assert_eq!(
            evaluate_applied(
                &sample(
                    2_000_000,
                    limits.temp_reduce_dc - TEMP_REDUCE_HYST_DC,
                    4_000_000
                ),
                &limits,
                2_000_000
            ),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );
    }

    #[test]
    fn idle_die_temperature_stays_below_the_reduce_threshold() {
        // Live measurement 19.09 at .657: the nabu die at idle (pump in standby,
        // input at the ADC floor) holds 43.5-46.1 °C, and under 2:1 load it reaches
        // 50.8 °C. The old thresholds 43.0/48.0/55.0 sat **below** idle, so the
        // fold-back fired constantly and the restore (threshold − 3.0 °C) was
        // unreachable: `IIN_CTRL` fell to `iin_floor_ua` (500 mA) and stayed there forever.
        const IDLE_MAX_DC: i32 = 461;
        const LOAD_MAX_DC: i32 = 508;
        let limits = GuardLimits::standard();
        assert!(
            limits.temp_reduce_dc > IDLE_MAX_DC,
            "fold-back threshold {} must be above the idle temperature {IDLE_MAX_DC}",
            limits.temp_reduce_dc
        );
        assert!(
            limits.temp_reduce_dc > LOAD_MAX_DC,
            "fold-back threshold {} must be above the working temperature {LOAD_MAX_DC}",
            limits.temp_reduce_dc
        );
        // The restore threshold must also be reachable on this board.
        assert!(
            limits.temp_reduce_dc - TEMP_REDUCE_HYST_DC > IDLE_MAX_DC,
            "restore threshold {} must be above the idle temperature {IDLE_MAX_DC}",
            limits.temp_reduce_dc - TEMP_REDUCE_HYST_DC
        );
    }

    #[test]
    fn threshold_set_applies_atomically_and_is_validated_once() {
        // A set that changes three thresholds at once must apply entirely: the old
        // `apply_parameter` checked consistency after every value and rejected the
        // very first one (`TempReduceDc` against the still old `TempBypassDc`).
        let mut limits = GuardLimits::standard();
        let before = limits;
        let set = [
            ("TempReduceDc", 560_u32),
            ("TempBypassDc", 620),
            ("TempStopDc", 680),
        ];
        for (name, value) in set {
            assert!(
                limits.apply_parameter_lenient(name, value),
                "{name} must pass over the range"
            );
        }
        assert!(limits.validate(), "a consistent set must pass the check");
        assert_eq!(limits.temp_reduce_dc, 560);
        assert_eq!(limits.temp_bypass_dc, 620);
        assert_eq!(limits.temp_stop_dc, 680);

        // Garbage is rejected element by element and at set validation too.
        let mut junk = before;
        assert!(!junk.apply_parameter_lenient("TempStopDc", 10_000));
        assert!(!junk.apply_parameter_lenient("NoSuchThreshold", 1));
        assert_eq!(junk, before, "garbage does not change the set");

        let mut broken = before;
        assert!(broken.apply_parameter_lenient("TempReduceDc", 600));
        assert!(broken.apply_parameter_lenient("TempBypassDc", 200));
        assert!(!broken.validate(), "a violated order must not pass");
    }

    #[test]
    fn converter_rail_artifact_neither_reduces_nor_locks_the_limit() {
        let limits = profile_limits();
        // Live case: Vin 9616 mV, VBAT 4780 mV, current 39 mA - this is the middle of
        // the switch-cap bus, not the cell: such a sample has nothing to cut.
        let artifact = sample(39_000, 350, 4_780_000);
        assert_eq!(
            evaluate_applied(&artifact, &limits, limits.restore_iin_ua()),
            GuardAction::None
        );
        // While the artefact holds, the limit must not lock at the floor: the sample
        // says nothing about the battery, so a return to the profile is allowed.
        assert_eq!(
            evaluate_applied(&artifact, &limits, limits.iin_floor_ua),
            GuardAction::RestoreCurrent {
                to_ua: limits.restore_iin_ua(),
                reason: "reduce_band_exit",
            }
        );
        // A plausible sample at the same Vin cuts current as usual.
        assert_eq!(
            evaluate_applied(
                &sample(2_800_000, 350, 4_460_000),
                &limits,
                limits.restore_iin_ua()
            ),
            GuardAction::ReduceCurrent {
                to_ua: 2_000_000,
                reason: "vbat_reduce",
            }
        );
    }

    #[test]
    fn full_charge_threshold_matches_nabu_non_ffc_and_ffc_targets() {
        // 4.42 V is the QC3 loop limit, not the full charge threshold: current must
        // not be cut on it. Non-FFC target 4.45 V, FFC 4.47 V (dtbo-03).
        let limits = GuardLimits::standard();
        assert_eq!(limits.vbat_reduce_uv, crate::encoding::NABU_VBAT_FLOAT_UV);
        assert!(limits.vbat_reduce_uv > crate::encoding::NABU_QC3_BAT_VOLT_MAX_UV);

        let mut t = profile_limits();
        assert!(t.apply_parameter("VbatReduceUv", crate::encoding::NABU_VBAT_NON_FFC_UV));
        assert_eq!(t.vbat_reduce_uv, 4_450_000);

        let profile = t.restore_iin_ua();
        // Below the threshold the guard is silent; at and above it cuts current.
        assert_eq!(
            evaluate_applied(&sample(1_500_000, 350, 4_440_000), &t, profile),
            GuardAction::None
        );
        assert_eq!(
            evaluate_applied(&sample(2_800_000, 350, 4_450_000), &t, profile),
            GuardAction::ReduceCurrent {
                to_ua: t.band_iin_ua(),
                reason: "vbat_reduce",
            }
        );
    }

    #[test]
    fn conservative_profile_keeps_its_own_taper_threshold() {
        // Strict profile - we do not touch it without justification: it is meant to cut early.
        assert_eq!(GuardLimits::conservative().vbat_reduce_uv, 4_350_000);
        assert!(
            GuardLimits::conservative().vbat_reduce_uv < GuardLimits::standard().vbat_reduce_uv
        );
    }

    #[test]
    fn power_absent_does_nothing() {
        let limits = GuardLimits::default();
        let mut absent = sample(0, 900, 4_000_000);
        absent.input_present = false;
        assert_eq!(
            evaluate_applied(&absent, &limits, limits.restore_iin_ua()),
            GuardAction::None
        );
    }

    #[test]
    fn conservative_profile_is_stricter() {
        let strict = GuardLimits::conservative();
        let normal = GuardLimits::default();
        assert!(strict.temp_reduce_dc < normal.temp_reduce_dc);
        assert!(strict.iin_max_ua < normal.iin_max_ua);
        let action = evaluate_applied(&sample(2_000_000, 420, 4_000_000), &strict, 1_500_000);
        assert!(action.is_change(), "the strict profile reacts earlier");
        assert_eq!(
            evaluate_applied(
                &sample(2_000_000, 420, 4_000_000),
                &normal,
                normal.restore_iin_ua()
            ),
            GuardAction::None,
            "the normal profile is still silent at 42.0 °C"
        );
    }

    #[test]
    fn bypass_resolution_is_allowed_only_on_the_five_volt_side() {
        let limits = GuardLimits::standard();
        // 5 V: 1:1 is allowed and the guard invents no fallback actions.
        assert_eq!(
            resolve_bypass(5_000_000, 4_000_000, 0, &limits),
            BypassResolution::Allowed
        );
        assert_eq!(
            resolve_bypass(7_999_999, 4_400_000, 5, &limits),
            BypassResolution::Allowed,
            "the denial counter must not obstruct a legitimate bypass"
        );
        // 9 V: 1:1 means 9 V on the battery. The first tick reduces current to the floor.
        assert_eq!(
            resolve_bypass(9_000_000, 4_275_000, 0, &limits),
            BypassResolution::ReduceCurrent {
                to_ua: limits.iin_floor_ua,
                reason: "die_temp_bypass_no_vin_headroom",
            }
        );
        // The temperature does not fall - the second tick stops the charge.
        assert_eq!(
            resolve_bypass(
                9_000_000,
                4_275_000,
                BYPASS_DENIED_STRIKES_BEFORE_STOP,
                &limits
            ),
            BypassResolution::Stop {
                reason: "die_temp_bypass_no_vin_headroom",
            }
        );
        // 8.0 V is already denied; an input below 4.2 V is too (1:1 is useless).
        for vin in [8_000_000, 8_416_000, 12_000_000] {
            assert!(
                !matches!(
                    resolve_bypass(vin, 4_200_000, 0, &limits),
                    BypassResolution::Allowed
                ),
                "Vin {vin} must not allow 1:1"
            );
        }
        assert!(!matches!(
            resolve_bypass(4_000_000, 4_000_000, 0, &limits),
            BypassResolution::Allowed
        ));
    }
    #[test]
    fn taper_setpoint_is_never_raised_by_the_guard() {
        // F10: the taper at the top of the charge writes 1.2 A past the profile (in
        // `config` 2.8 A remains). A decision "by the profile" would raise current to
        // the 2.0 A band, that is, the guard would act in the opposite direction.
        let limits = profile_limits();
        let tapered = sample(1_200_000, limits.temp_reduce_dc + 1, 4_460_000);
        assert_eq!(
            evaluate(
                &tapered,
                &limits,
                Some(crate::encoding::VBAT_TAPER_IIN_UA),
                None
            ),
            GuardAction::None,
            "1.2 A is below the fold-back band: the guard has no right to raise current"
        );
        // An even lower setpoint - also nothing: the fold-back only reduces.
        assert_eq!(
            evaluate(&tapered, &limits, Some(1_000_000), None),
            GuardAction::None
        );
        // With a setpoint above the band the decision is usual - fold-back to 2.0 A.
        assert_eq!(
            evaluate(&tapered, &limits, Some(2_800_000), None),
            GuardAction::ReduceCurrent {
                to_ua: 2_000_000,
                reason: "die_temp_reduce",
            }
        );
    }

    #[test]
    fn restore_stops_at_the_deliberate_taper_setpoint() {
        // F15: the taper band (Vbat >= 4.37 V at a 4.47 V target) overlaps the
        // restore band (Vbat <= 4.40 V at a 4.45 V threshold). In that window the
        // guard, seeing the low 1.2 A setpoint, restored the profile 2.8 A, that is,
        // it cancelled the deliberate taper reduction every 250 ms.
        let limits = profile_limits();
        let taper = sample(1_200_000, 400, 4_380_000);
        assert_eq!(
            evaluate(
                &taper,
                &limits,
                Some(crate::encoding::VBAT_TAPER_IIN_UA),
                Some(crate::encoding::VBAT_TAPER_IIN_UA)
            ),
            GuardAction::None,
            "a low current inside the taper band is deliberate: the restore must not raise it"
        );

        // Control: without a deliberate setpoint (the taper does not drive current) the
        // return to the profile must work - the ceiling must not disable the guard itself.
        assert_eq!(
            evaluate(&taper, &limits, Some(1_200_000), None),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            },
            "the guard without a taper returns the profile as before"
        );

        // And outside the taper band (Vbat below the threshold) the restore also fires.
        let below_band = sample(1_200_000, 400, 4_200_000);
        assert_eq!(
            evaluate(&below_band, &limits, Some(1_200_000), None),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );

        // Ceiling: the restore raises the limit only **up to** the deliberate setpoint,
        // not above, and never lowers it (lowering is the fold-back paths' job).
        assert_eq!(
            evaluate(&below_band, &limits, Some(500_000), Some(1_200_000)),
            GuardAction::RestoreCurrent {
                to_ua: 1_200_000,
                reason: "reduce_band_exit",
            },
            "the restore ceiling is the deliberate setpoint, not the profile"
        );
        assert_eq!(
            evaluate(&below_band, &limits, Some(1_200_000), Some(1_200_000)),
            GuardAction::None,
            "at the ceiling there is nothing to restore"
        );
    }

    #[test]
    fn unread_setpoint_defers_current_decisions() {
        // F10: the `IIN_CTRL` register was not read - the actual setpoint is unknown.
        // We do not substitute the profile for it: there are no current decisions in
        // this tick, but the thresholds on a valid temperature keep working.
        let limits = profile_limits();
        assert_eq!(
            evaluate(&sample(2_800_000, 440, 4_460_000), &limits, None, None),
            GuardAction::None,
            "a fold-back without a setpoint is not counted"
        );
        assert_eq!(
            evaluate(&sample(4_000_000, 350, 4_000_000), &limits, None, None),
            GuardAction::None,
            "overcurrent without a setpoint is not cut either"
        );
        assert_eq!(
            evaluate(
                &sample(2_800_000, limits.temp_bypass_dc, 4_000_000),
                &limits,
                None,
                None
            ),
            GuardAction::FallbackToBypass {
                reason: "die_temp_bypass"
            },
            "the 1:1 threshold does not depend on the setpoint"
        );
        assert_eq!(
            evaluate(
                &sample(2_800_000, limits.temp_stop_dc, 4_000_000),
                &limits,
                None,
                None
            ),
            GuardAction::Stop {
                reason: "die_temp_stop"
            }
        );
    }

    #[test]
    fn invalid_channels_never_change_the_current() {
        // F11: a channel failure is indistinguishable by value from an honest zero, so
        // an invalid tick gives neither fold-back nor restore - only `None`.
        let limits = profile_limits();
        let profile = limits.restore_iin_ua();

        // VBAT not read (0 µV): neither a return to the profile nor a fold-back.
        let mut bad_vbat = sample(2_000_000, 350, 0);
        bad_vbat.vbat_valid = false;
        assert_eq!(
            evaluate(&bad_vbat, &limits, Some(limits.iin_floor_ua), None),
            GuardAction::None,
            "a return to the profile is forbidden on a failed battery channel"
        );
        assert_eq!(
            evaluate(&bad_vbat, &limits, Some(profile), None),
            GuardAction::None,
            "and we do not invent a fold-back on zero"
        );

        // DieTemp not read, and the value is a raw zero (the decoder gives +160.0 °C).
        let mut bad_temp = sample(2_000_000, 0, 4_000_000);
        bad_temp.die_temp_valid = false;
        assert_eq!(
            evaluate(&bad_temp, &limits, Some(limits.iin_floor_ua), None),
            GuardAction::None
        );
        assert_eq!(
            evaluate(&bad_temp, &limits, Some(profile), None),
            GuardAction::None
        );

        // The channel was read, but the value is implausible (`AdcChannel::DieTemp`
        // returns 160.0 °C on a zero code): the safety limit does not fire on garbage.
        let garbage = sample(2_000_000, 1_600, 4_000_000);
        assert!(garbage.die_temp_valid, "the channel still counts as read");
        assert!(!die_temp_usable(&garbage));
        assert_eq!(
            evaluate(&garbage, &limits, Some(profile), None),
            GuardAction::None,
            "an invalid temperature does not stop the charge"
        );
        assert_eq!(
            evaluate(&garbage, &limits, Some(limits.iin_floor_ua), None),
            GuardAction::None
        );
    }

    #[test]
    fn valid_cold_tick_restores_the_profile_limit() {
        // F11, the other side: both channels are alive and it is cold - the restore works,
        // the safety limit on a true zero is not disabled (0.0 °C is below the thresholds).
        let limits = profile_limits();
        let cold = sample(2_000_000, 0, 4_000_000);
        assert!(cold.vbat_valid && cold.die_temp_valid);
        assert!(die_temp_usable(&cold));
        assert_eq!(
            evaluate(&cold, &limits, Some(limits.iin_floor_ua), None),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );
        // The same sample, but hot - no restore, and the setpoint goes to the band.
        assert_eq!(
            evaluate(
                &sample(2_000_000, limits.temp_reduce_dc + 1, 4_000_000),
                &limits,
                Some(2_800_000),
                None
            ),
            GuardAction::ReduceCurrent {
                to_ua: 2_000_000,
                reason: "die_temp_reduce",
            }
        );
    }

    #[test]
    fn two_heat_episodes_behave_identically_after_the_strike_reset() {
        // F12: the 1:1 denial counter is zeroed on leaving the band. Without the reset
        // a second episode ≥ 48 °C would stop the charge on its first tick, skipping
        // the current reduction step.
        let limits = profile_limits();
        let bypass = limits.temp_bypass_dc;
        let reset = bypass - TEMP_REDUCE_HYST_DC;
        assert!(
            !bypass_strikes_expired(&sample(2_800_000, bypass, 4_250_000), &limits),
            "inside the band the counter is not reset"
        );
        // At 2.0 °C above the reset threshold - still inside the hysteresis band.
        assert!(!bypass_strikes_expired(
            &sample(2_000_000, reset + 20, 4_250_000),
            &limits
        ));
        assert!(
            bypass_strikes_expired(&sample(2_000_000, reset, 4_250_000), &limits),
            "the reset threshold is leaving the overtemperature band"
        );
        // An invalid temperature does not count as a reset.
        let mut bad = sample(2_000_000, 200, 4_250_000);
        bad.die_temp_valid = false;
        assert!(!bypass_strikes_expired(&bad, &limits));

        let mut denied_strikes = 0;
        for episode in 0..2 {
            assert_eq!(
                resolve_bypass(9_000_000, 4_250_000, denied_strikes, &limits),
                BypassResolution::ReduceCurrent {
                    to_ua: limits.iin_floor_ua,
                    reason: "die_temp_bypass_no_vin_headroom",
                },
                "episode {episode}: the first tick is the current reduction step"
            );
            denied_strikes += 1;
            assert_eq!(
                resolve_bypass(9_000_000, 4_250_000, denied_strikes, &limits),
                BypassResolution::Stop {
                    reason: "die_temp_bypass_no_vin_headroom",
                },
                "episode {episode}: the second tick is the charge stop"
            );
            // This is how KMDF does it before the guard decision.
            if bypass_strikes_expired(&sample(2_000_000, reset, 4_250_000), &limits) {
                denied_strikes = 0;
            }
        }
        assert_eq!(
            denied_strikes, 0,
            "both episodes start from zero denials and behave identically"
        );
    }
}
