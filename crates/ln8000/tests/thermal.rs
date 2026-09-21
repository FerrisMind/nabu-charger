//! Heating scenario from cold to stop: all three protection levels.
//!
//! The test repeats the driver's path: it reads the temperature from the ADC, decides
//! with the guard and **applies that decision to the chip**, then checks the registers.
//! Not a check of one formula, but a cross-check of "sensor → decision → write".
//!
//! A move to 1:1 is permitted only for Vin inside the bypass window: at a raised
//! input the guard cuts current and then stops charging (a separate scenario).
//!
//! The steps come from the profile thresholds (`GuardLimits::standard`) and are not
//! hard-coded numbers: the thresholds must sit above this board's crystal idle
//! temperature (live measurement 19.09 - 46.1 °C at idle), and hard-coded values
//! drifted apart from them silently.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use ln8000::testkit::MockPumpBus;
use ln8000::{
    AdcChannel, BypassResolution, GuardAction, GuardLimits, OpMode, Pump, PumpConfig,
    TEMP_REDUCE_HYST_DC, TelemetrySample, evaluate, regs, resolve_bypass,
};

/// Writes the die temperature into the ADC registers the way the chip would.
///
/// The temperature is given as a channel code: dC = (935 - code) * 4350 / 1000,
fn set_die_temp(pump: &mut Pump<MockPumpBus>, deci_celsius: i32) {
    // Inverse conversion: code = 935 - (dC * 1000) / 4350,
    // that is raw = 935 - (dC * 1000) / 4350. The value is clamped to the ADC range.
    let code = (935_i32 - (deci_celsius * 1000) / 4350).clamp(0, 1_023);
    let register = AdcChannel::DieTemp.register();
    let bus = pump.bus_mut();
    // The channel code is packed into the common bit stream: 6 code bits sit in the
    // low byte shifted by 2, the top 4 bits in the low bits of the next one.
    bus.set_reg(register, u8::try_from((code % 64) * 4).unwrap_or(0));
    bus.set_reg(register + 1, u8::try_from((code / 64) & 0x0F).unwrap_or(0));
}

/// Builds a telemetry sample: the temperature is read from the chip, the other
/// channels are fixed and plausible.
///
/// This is deliberate: the ADC register pairs overlap (battery voltage is read from
/// `0x0E–0x0F`, temperature from `0x0F–0x10`), so substituting every channel via
/// the mock would give a garbage sample. The scenario checks the temperature and
/// keeps current and voltage in the normal range so that only it can interfere.
fn sample_from(pump: &mut Pump<MockPumpBus>, now_ms: u64) -> TelemetrySample {
    sample_from_with_vin(pump, now_ms, 9_000_000)
}

/// [`sample_from`] with an explicit Vin: the 1:1 bypass gate depends on the input voltage.
fn sample_from_with_vin(pump: &mut Pump<MockPumpBus>, now_ms: u64, vin_uv: u32) -> TelemetrySample {
    let temp = pump.read_adc(AdcChannel::DieTemp).expect("die temperature");
    TelemetrySample {
        ts_ms: now_ms,
        vbat_uv: 3_900_000,
        vbus_uv: vin_uv,
        iin_ua: 2_000_000,
        die_temp_dc: temp,
        op_mode: pump.op_mode(),
        input_present: true,
        // The temperature was just read from the chip; VBAT in this scenario is a
        // plausible constant, not the result of an ADC read.
        vbat_valid: true,
        die_temp_valid: true,
    }
}

/// Overtemperature at a raised Vin (live QC/PD scenario, 9 V).
///
/// 1:1 here means 9 V on the battery, so the guard must cut current instead of
/// bypassing, and stop charging at a persistent temperature. The `EN_1TO1` bit
/// must not appear at any step.
#[test]
// One ramp end to end: splitting it would hide the step sequence the test asserts on.
#[allow(clippy::too_many_lines)]
fn thermal_ramp_on_elevated_vin_never_uses_bypass() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("chip open");
    pump.configure().expect("threshold configuration");
    assert_eq!(
        pump.enable_switching().expect("2:1 mode"),
        OpMode::Switching
    );

    // Snapshot of the profile limit, as in `read_parameters`: the guard brings
    // current back exactly to it, and without the snapshot it would "restore" to 2 A.
    let mut limits = GuardLimits::standard();
    let target_before = pump.config().iin_limit_ua;
    limits.iin_profile_ua = target_before;

    // Temperature rises: normal → current reduction → 1:1 refused (reduce) → stop.
    // The steps are derived from the profile thresholds: hard-coded numbers drifted
    // apart from them silently (the thresholds sit above crystal idle - 46.1 °C).
    let ramp = [
        limits.temp_reduce_dc - 100,
        limits.temp_reduce_dc + 10,
        limits.temp_bypass_dc + 10,
        limits.temp_stop_dc + 10,
    ];
    let mut seen: Vec<&'static str> = Vec::new();
    let mut denied_strikes = 0_u32;

    for (index, temperature) in ramp.iter().enumerate() {
        set_die_temp(&mut pump, *temperature);

        let sample = sample_from(&mut pump, u64::try_from(index).unwrap_or(0) * 1_000);
        // The ADC quantises the value, so we compare with a tolerance, not exactly.
        assert!(
            (sample.die_temp_dc - temperature).abs() <= 5,
            "the temperature read {} must be close to the written {}",
            sample.die_temp_dc,
            temperature
        );

        // There is no deliberate setpoint (taper) in this scenario: Vbat is far from the band.
        let action = evaluate(
            &sample,
            &limits,
            pump.applied_iin_ua(),
            pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, false),
        );
        match action {
            GuardAction::None => {
                denied_strikes = 0;
                seen.push("normal");
            }
            GuardAction::ReduceCurrent { to_ua, .. } => {
                denied_strikes = 0;
                seen.push("current reduction");
                pump.set_iin_limit(to_ua)
                    .expect("current reduction in the chip");
            }
            GuardAction::FallbackToBypass { .. } => {
                // Exactly what KMDF does: the decision is resolved with Vin taken into account.
                match resolve_bypass(
                    i32::try_from(sample.vbus_uv).expect("Vin into i32"),
                    sample.vbat_uv,
                    denied_strikes,
                    &limits,
                ) {
                    BypassResolution::Allowed => panic!("1:1 at 9 V is not allowed"),
                    BypassResolution::ReduceCurrent { to_ua, .. } => {
                        denied_strikes = denied_strikes.saturating_add(1);
                        seen.push("1:1 refused -> current reduction");
                        pump.set_iin_limit(to_ua)
                            .expect("current reduction in the chip");
                    }
                    BypassResolution::Stop { .. } => {
                        seen.push("1:1 refused -> stop");
                        pump.standby().expect("charge stop");
                    }
                    // The enum is marked `non_exhaustive`.
                    _ => seen.push("other"),
                }
            }
            GuardAction::Stop { .. } => {
                seen.push("stop");
                pump.standby().expect("charge stop");
            }
            // The enum is marked `non_exhaustive`: the guard emits no new variants yet.
            _ => seen.push("other"),
        }

        assert_eq!(
            pump.bus().reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "at a raised Vin the 1:1 bit must not appear (step {temperature})"
        );
    }

    assert_eq!(
        seen,
        vec![
            "normal",
            "current reduction",
            "1:1 refused -> current reduction",
            "stop"
        ],
        "at a raised Vin the guard does not go to 1:1, it cuts current and stops"
    );

    // What actually happened to the chip by the end of the scenario.
    let sys_ctrl = pump.bus().reg(regs::SYS_CTRL);
    assert_ne!(
        sys_ctrl & (1 << 3),
        0,
        "by the end of the scenario SYS_CTRL must have the standby bit set"
    );
    assert_eq!(
        pump.op_mode(),
        OpMode::Standby,
        "standby mode is expected after a stop"
    );
    assert!(
        pump.config().iin_limit_ua <= target_before,
        "the current limit must not grow from the guard's actions"
    );
}

/// Puts a Vin value into the ADC registers so that `read_adc(Vin)` returns ≈ `uv`.
fn set_vin_adc(pump: &mut Pump<MockPumpBus>, uv: i32) {
    let units = u16::try_from((uv / 16_000).clamp(0, 1023)).unwrap_or(0);
    let high = u8::try_from((units / 16) & 0x3F).unwrap_or(0);
    let low = u8::try_from((units % 16) * 16).unwrap_or(0);
    let register = AdcChannel::Vin.register();
    pump.bus_mut().set_reg(register, low);
    pump.bus_mut().set_reg(register + 1, high);
}

/// The same overtemperature, but with the input in the bypass window (5 V): 1:1 is allowed.
#[test]
fn thermal_bypass_on_five_volts_is_allowed() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("chip open");
    pump.configure().expect("threshold configuration");
    pump.enable_switching().expect("2:1 mode");

    let mut limits = GuardLimits::standard();
    limits.iin_profile_ua = pump.config().iin_limit_ua;
    set_die_temp(&mut pump, limits.temp_bypass_dc + 10);
    set_vin_adc(&mut pump, 5_000_000);
    let sample = sample_from_with_vin(&mut pump, 0, 5_000_000);
    assert!(matches!(
        evaluate(
            &sample,
            &limits,
            pump.applied_iin_ua(),
            pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, false),
        ),
        GuardAction::FallbackToBypass { .. }
    ));
    assert_eq!(
        resolve_bypass(5_000_000, sample.vbat_uv, 0, &limits),
        BypassResolution::Allowed
    );
    pump.enable_bypass().expect("move to bypass at 5 V");
    assert_eq!(
        pump.bus().reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
        regs::SYS_CTRL_EN_1TO1,
        "at 5 V the bypass must engage"
    );
}

#[test]
fn cooled_down_chip_returns_to_switching() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("chip open");
    pump.configure().expect("configuration");
    pump.enable_switching().expect("2:1 mode");

    let mut limits = GuardLimits::standard();
    limits.iin_profile_ua = pump.config().iin_limit_ua;

    // Overtemperature first: we enter protection.
    set_die_temp(&mut pump, limits.temp_stop_dc + 10);
    let hot = sample_from(&mut pump, 0);
    assert!(matches!(
        evaluate(
            &hot,
            &limits,
            pump.applied_iin_ua(),
            pump.taper_setpoint_ua(hot.vbat_uv, hot.vbus_uv, false),
        ),
        GuardAction::Stop { .. }
    ));
    pump.standby().expect("stop");

    // Then cooling: the chip can be brought back to a working mode.
    set_die_temp(&mut pump, limits.temp_reduce_dc - TEMP_REDUCE_HYST_DC - 100);
    let cold = sample_from(&mut pump, 1_000);
    assert!(
        matches!(
            evaluate(
                &cold,
                &limits,
                pump.applied_iin_ua(),
                pump.taper_setpoint_ua(cold.vbat_uv, cold.vbus_uv, false),
            ),
            GuardAction::None
        ),
        "after cooling the guard must not interfere"
    );
    pump.configure().expect("reconfiguration");
    assert_eq!(
        pump.enable_switching().expect("return to 2:1"),
        OpMode::Switching,
        "after cooling the mode must recover"
    );
}
