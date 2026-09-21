//! Pure `BattC` power-state hysteresis (no WDF, host-testable).
//!
//! KMDF `battery.rs` calls this from `update_from_telemetry` so that one ADC
//! tick cannot flap the tray icon. Every QC3 pulse, every charge-pump mode
//! transition and every fallback to 5 V drops `Iin` to the 39 mA ADC floor
//! (8 × 4.89 mA) and/or moves `Vin` for a tick; the live symptom was the tray
//! switching "charging" / "not charging" in bursts while the percentage still
//! rose — Kernel-Power 105 (power source change) fired 11 times in 12 seconds.
//!
//! Two held flags are enough: `online` (`ONLINE_HOLD_MS`) and `charging`
//! (`CHARGING_HOLD_MS`, longer because the ADC floor lasts through mode
//! transitions). `DISCHARGING` is derived from the held online value, so a
//! one-tick dropout never shows "discharging".

use crate::encoding::vin_is_doubled_vbat;

/// Once AC is online, keep reporting online until the raw predicate has been
/// false continuously for this long (ms).
pub const ONLINE_HOLD_MS: u64 = 8_000;
/// Once charging, keep reporting charging until the charging predicate has been
/// false continuously for this long (ms).
pub const CHARGING_HOLD_MS: u64 = 20_000;
/// Charging current floor (µA) for the raw charging predicate.
///
/// LN8000 `Iin` reads 39 mA on the ADC floor, so a single tick at the floor is
/// not evidence that charging stopped; below this the raw predicate is false
/// and only the hold keeps `CHARGING` published.
pub const IIN_CHARGING_UA: u32 = 80_000;

/// Held boolean plus the timestamp of the last raw `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// Last reported (held) value.
    pub held: bool,
    /// Monotonic milliseconds of the last raw `true`.
    pub last_true_ms: u64,
    /// Consecutive raw `true` samples seen so far.
    true_run: u32,
}

/// Consecutive raw `true` samples required to arm or extend a hold.
///
/// One lone `true` between `false` samples must not re-arm the window: a raw
/// predicate flapping faster than `hold_ms` would otherwise pin the flag
/// forever without any sustained evidence. With this requirement the flag is
/// bounded — it clears `hold_ms` after the last *run* of [`HOLD_ARM_RUN`]
/// consecutive true samples.
pub const HOLD_ARM_RUN: u32 = 2;

impl Hold {
    /// Never-held state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: false,
            last_true_ms: 0,
            true_run: 0,
        }
    }

    /// Applies one sample of the raw predicate.
    ///
    /// `raw` true for [`HOLD_ARM_RUN`] consecutive samples → held, and the
    /// timestamp moves to `now_ms` (a fresh run re-arms the full hold window).
    /// `raw` false while held → stays held while `now_ms - last_true_ms <
    /// hold_ms`; at `hold_ms` and beyond the hold ends. Timestamps are
    /// monotonic; a non-monotonic `now_ms` keeps the hold (saturating
    /// difference is 0).
    #[must_use]
    pub const fn update(self, raw: bool, now_ms: u64, hold_ms: u64) -> Self {
        if raw {
            let true_run = self.true_run.saturating_add(1);
            if true_run >= HOLD_ARM_RUN {
                Self {
                    held: true,
                    last_true_ms: now_ms,
                    true_run,
                }
            } else {
                // First sample of a run: not evidence yet, do not extend.
                Self {
                    held: self.held,
                    last_true_ms: self.last_true_ms,
                    true_run,
                }
            }
        } else if self.held && now_ms.saturating_sub(self.last_true_ms) < hold_ms {
            Self {
                held: self.held,
                last_true_ms: self.last_true_ms,
                true_run: 0,
            }
        } else {
            Self {
                held: false,
                last_true_ms: self.last_true_ms,
                true_run: 0,
            }
        }
    }

    /// Applies one telemetry tick that may carry **no evidence at all**.
    ///
    /// [`online_raw_with_evidence`] returns `None` when the tick had no usable
    /// input measurement *and* the hardware did not say VBUS was gone — a
    /// hibernated ADC is not a cable pull. `None` leaves the hold exactly as it
    /// was, so a run of such ticks can never expire it: neither [`Self::held`]
    /// nor [`Self::last_true_ms`] moves, and the arming run is not advanced
    /// either. `Some(raw)` behaves exactly like [`Self::update`].
    #[must_use]
    pub const fn update_evidence(self, evidence: Option<bool>, now_ms: u64, hold_ms: u64) -> Self {
        match evidence {
            Some(raw) => self.update(raw, now_ms, hold_ms),
            // A tick without evidence moves neither the flag, the timestamp nor
            // the arming run: "do not know" is not "no adapter".
            None => self,
        }
    }
}

impl Default for Hold {
    fn default() -> Self {
        Self::new()
    }
}

/// Raw charging predicate before hysteresis: the adapter is held online and
/// either the instantaneous `Iin` or the window peak is at/above [`IIN_CHARGING_UA`].
///
/// The peak matters because a single tick can read the ADC floor while the
/// window as a whole is charging.
#[must_use]
pub const fn charging_raw(online_held: bool, iin_ua: u32, iin_peak_ua: u32) -> bool {
    online_held && (iin_ua >= IIN_CHARGING_UA || iin_peak_ua >= IIN_CHARGING_UA)
}

/// True adapter / USB rail floor (µV) when no current is flowing.
///
/// Must sit **above** Li-ion OCV: with the cable unplugged, LN8000 Vin often
/// tracks VBAT while in bypass (~4.2–4.4 V), and a lower floor left the tray
/// showing "charging" forever.
pub const VBUS_ONLINE_UV: u32 = 4_600_000;
/// Vin above which delivered current alone proves an adapter (µV).
///
/// Below this the 1:1 bypass path is not even selectable
/// (`CHARGE_MIN_VIN_UV` in `encoding`).
pub const VBUS_CHARGING_MIN_UV: u32 = 4_200_000;
/// Vin must exceed VBAT by this much below [`VBUS_ELEVATED_UV`] (µV).
pub const VBUS_ABOVE_VBAT_UV: u32 = 200_000;
/// Elevated QC/PD rail — always AC even if VBAT is high (µV).
pub const VBUS_ELEVATED_UV: u32 = 6_000_000;

/// Raw (un-held) "adapter present" decision for one telemetry tick.
///
/// Vin alone is wrong in both directions:
/// - unplugged bypass leaves Vin ≈ VBAT above a naive 4.2 V floor;
/// - a 5 V block sagging under a 2 A load reads **below** the 4.6 V floor while
///   real current flows into the pack. Live 18.09: `Vin=4 384 000`,
///   `Iin=2 063 580` and the tray showed "on battery" while the pack charged at
///   2 A — Windows then applied the DC idle/sleep policy to a tablet that was
///   sitting on a brick.
///
/// Current into the pack can only come from an adapter, so `Iin` at/above the
/// charging floor is adapter evidence on its own. Unplugged input reads the
/// 39 mA ADC floor, far below [`IIN_CHARGING_UA`], so the old guard still holds.
///
/// `vac_unplug`: `FAULT1` bit 4 (`LN8000_MASK_VAC_UNPLUG_STS`, vendor
/// `ln8000_charger.h:62`). The vendor answers **with this bit** the question "is
/// VBUS" (`POWER_SUPPLY_PROP_TI_VBUS_PRESENT` → `!vac_unplug`,
/// `ln8000_charger.c:948`), so unplug comes from the hardware, not from the ADC.
/// Live measurement 19.09 with the cable unplugged: `FAULT1 = 0x30` (bit 4 set),
/// `Vin = 8.80 V` with the cell at `4.40 V`, current 39 mA.
///
/// The order of the checks matters. Current comes first: it overrides both the
/// `VAC_UNPLUG` latch and the "doubled" bus - if 2 A flows into the pack, the
/// adapter is there, whatever `Vin` looks like. Next the cell reflection through
/// the converter bus is cut off: without a cable the `VIN` node is unloaded, and
/// the ADC reads exactly `2 · VBAT`. That reflection scales with the cell, so the
/// check must come **before** the `Vin ≥ 6 V` branch, which would otherwise
/// accept it unconditionally.
#[must_use]
pub const fn online_raw(vbus_uv: u32, vbat_uv: u32, iin_ua: u32, vac_unplug: bool) -> bool {
    if vbus_uv >= VBUS_CHARGING_MIN_UV && iin_ua >= IIN_CHARGING_UA {
        return true;
    }
    if vac_unplug {
        return false;
    }
    if vbus_uv >= VBUS_ONLINE_UV && vin_is_doubled_vbat(vbat_uv, vbus_uv) {
        return false;
    }
    if vbus_uv >= VBUS_ELEVATED_UV {
        return true;
    }
    if vbus_uv < VBUS_ONLINE_UV {
        return false;
    }
    // 4.6–6.0 V: require Vin clearly above pack voltage so VBAT float ≠ AC.
    if vbat_uv > 0 && vbus_uv < vbat_uv.saturating_add(VBUS_ABOVE_VBAT_UV) {
        return false;
    }
    true
}

/// [`online_raw`] for one tick **together with that tick's authority to age the
/// hold**.
///
/// A zero in this telemetry is not "0 V" — it is "no sample", and it arrives by
/// two different routes:
///
/// * a bus read failure collapses to zero at the call site (`vbat_read
///   .unwrap_or_default()` in the KMDF tick, `ln8000-kmdf/src/lib.rs:2441-2443`);
/// * the LN8000 ADC auto-hibernates: init step 9 writes `ADC_CTRL` bits 5:7 =
///   `AutoHibernate` with `Sec4` (`ln8000/src/driver.rs:497-508`), after which
///   `ADC01..ADC09` read **successfully** but contain `0x00` in every channel
///   (`encoding::vbat_reading_usable`, `ln8000/src/encoding.rs:279-293`).
///
/// Fed those zeros, the Vin chain in [`online_raw`] falls into `vbus_uv <
/// VBUS_ONLINE_UV → false`, so every unreadable tick reads as "the adapter is
/// gone" and ages the hold (`battery.rs` calls `Hold::update` on every tick with
/// a fresh `now_ms`). Because `ONLINE_HOLD_MS` (8 s) is shorter than
/// `CHARGING_HOLD_MS` (20 s), a pump-idle stretch longer than ~4 s of
/// hibernation plus the 8 s window publishes AC → DC → AC to Windows while the
/// brick never moved — the exact shape of a phantom power-source change.
///
/// So the tick's validity is part of the decision:
///
/// * `input_readings_usable` → `Some(online_raw(..))`: a real measurement, and
///   the existing chain decides;
/// * not usable and `vac_unplug` set → `Some(false)`: the hardware itself says
///   VBUS is gone (`FAULT1` bit 4, read fresh from the chip in the same tick at
///   `ln8000-kmdf/src/lib.rs:2453`), so the hold must age exactly as before;
/// * not usable and `vac_unplug` clear → `None`: no measurement and no hardware
///   verdict. The caller must **leave the hold untouched**
///   ([`Hold::update_evidence`]) — an unreadable tick is not evidence of absence.
///
/// `vac_unplug` is deliberately trusted only in the "not usable" case: while a
/// real `Vin`/`Iin` measurement exists, [`online_raw`] keeps its current
/// precedence (current first, then the bit) and its behaviour is untouched.
#[must_use]
pub const fn online_raw_with_evidence(
    vbus_uv: u32,
    vbat_uv: u32,
    iin_ua: u32,
    vac_unplug: bool,
    input_readings_usable: bool,
) -> Option<bool> {
    if input_readings_usable {
        return Some(online_raw(vbus_uv, vbat_uv, iin_ua, vac_unplug));
    }
    if vac_unplug {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Telemetry period (ms) — the timer's default tick.
    const TICK_MS: u64 = 250;
    /// LN8000 `Iin` ADC floor (µA): 8 × 4.89 mA.
    const IIN_FLOOR_UA: u32 = 39_000;

    /// Arms a hold the way the telemetry tick does: [`HOLD_ARM_RUN`] consecutive
    /// raw-true samples.
    fn arm(raw: bool, hold_ms: u64) -> Hold {
        let mut st = Hold::new();
        for i in 0..HOLD_ARM_RUN {
            st = st.update(raw, u64::from(i) * TICK_MS, hold_ms);
        }
        st
    }

    #[test]
    fn one_low_current_tick_keeps_charging() {
        // t = 0/250 ms: 3 A, charging armed (armed_at = 250 ms).
        let st = arm(true, CHARGING_HOLD_MS);
        assert!(st.held);
        // t = 500 ms: one QC3-pulse tick on the ADC floor must not clear CHARGING.
        let st = st.update(
            charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA),
            500,
            CHARGING_HOLD_MS,
        );
        assert!(st.held, "one tick at the ADC floor does not clear CHARGING");
    }

    #[test]
    fn sustained_low_current_clears_charging() {
        let armed_at = 250u64;
        let mut st = arm(true, CHARGING_HOLD_MS);
        let mut now = armed_at;
        // Still held just before the 20 s boundary after the last real run.
        while now < armed_at + 19_750 {
            now += TICK_MS;
            st = st.update(
                charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA),
                now,
                CHARGING_HOLD_MS,
            );
        }
        assert!(st.held, "the hold survives up to 20 s");
        // 25 s of sustained low current: cleared.
        while now < armed_at + 25_000 {
            now += TICK_MS;
            st = st.update(
                charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA),
                now,
                CHARGING_HOLD_MS,
            );
        }
        assert!(!st.held, "25 s of low current clear CHARGING");
    }

    #[test]
    fn one_offline_tick_keeps_ac_online() {
        let st = arm(true, ONLINE_HOLD_MS);
        assert!(st.held);
        // One tick below the Vin threshold: online stays held, so battery.rs
        // (DISCHARGING = !online_held) never publishes DISCHARGING.
        let st = st.update(false, 500, ONLINE_HOLD_MS);
        assert!(st.held, "one offline tick must not give DISCHARGING");
    }

    #[test]
    fn sustained_offline_clears_online() {
        let armed_at = 250u64;
        let mut st = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        while now < armed_at + 7_750 {
            now += TICK_MS;
            st = st.update(false, now, ONLINE_HOLD_MS);
        }
        assert!(st.held, "the hold survives up to 8 s");
        while now < armed_at + 9_000 {
            now += TICK_MS;
            st = st.update(false, now, ONLINE_HOLD_MS);
        }
        assert!(!st.held, "9 s offline clear online_held");
    }

    #[test]
    fn window_peak_above_floor_keeps_charging() {
        // Instantaneous sample on the ADC floor, but the window peak is 2 A:
        // the raw predicate is still true, so nothing depends on the hold.
        assert!(charging_raw(true, IIN_FLOOR_UA, 2_000_000));
        let st = arm(true, CHARGING_HOLD_MS);
        assert!(st.held);
        // Both readings on the floor: the raw predicate is false and only the
        // hold keeps CHARGING published.
        assert!(!charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA));
        // Offline: a peak current cannot make charging true.
        assert!(!charging_raw(false, 2_000_000, 2_000_000));
    }

    #[test]
    fn single_true_sample_never_arms() {
        // A lone tick of "evidence" between false samples is not a run.
        let st = Hold::new().update(true, 0, ONLINE_HOLD_MS);
        assert!(!st.held, "a single true does not arm the hold");
        let st = st.update(false, TICK_MS, ONLINE_HOLD_MS);
        let st = st.update(true, 2 * TICK_MS, ONLINE_HOLD_MS);
        assert!(!st.held, "true/false chatter does not arm the hold");
        // Two consecutive true samples do arm it.
        let st = st.update(true, 3 * TICK_MS, ONLINE_HOLD_MS);
        assert!(st.held, "two true samples in a row arm the hold");
    }

    #[test]
    fn flapping_raw_never_pins_the_hold_forever() {
        // Raw predicate toggling every tick: the hold must still end one window
        // after the last *run* of evidence, not stay latched forever.
        let armed_at = 250u64;
        let mut st = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        // Start with a `false` sample so the arming run is broken: from then on
        // no two consecutive samples are both true.
        let mut raw = true;
        while now < armed_at + 4_000 {
            now += TICK_MS;
            raw = !raw;
            st = st.update(raw, now, ONLINE_HOLD_MS);
        }
        assert!(
            st.held,
            "chatter inside the window does not clear the hold early"
        );
        while now <= armed_at + ONLINE_HOLD_MS {
            now += TICK_MS;
            st = st.update(false, now, ONLINE_HOLD_MS);
        }
        assert!(!st.held, "after the last sustained run the hold must end");
    }

    #[test]
    fn sagging_five_volt_rail_with_current_is_online() {
        // Live 18.09 acceptance: 2.06 A into the pack at Vin 4.384 V. The old
        // 4.6 V floor called this "on battery" while it charged.
        assert!(online_raw(4_384_000, 4_300_000, 2_063_580, false));
        // Same rail, current tapered to the ADC floor: back to the Vin rules.
        assert!(!online_raw(4_384_000, 4_300_000, IIN_FLOOR_UA, false));
        // A dead bus cannot deliver current.
        assert!(!online_raw(0, 4_300_000, 2_000_000, false));
        // Below the bypass floor: not an adapter even with a suspicious sample.
        assert!(!online_raw(3_900_000, 3_800_000, 2_000_000, false));
    }

    #[test]
    fn unplugged_bypass_is_not_online() {
        // Unplugged, LN8000 Vin tracks VBAT in bypass and input reads the floor.
        assert!(!online_raw(4_250_000, 4_250_000, IIN_FLOOR_UA, false));
        assert!(!online_raw(4_384_000, 4_380_000, IIN_FLOOR_UA, false));
    }

    #[test]
    fn elevated_rail_is_online_unless_it_is_the_pack_reflection() {
        // 2:1 charge-pump band, 1.9 A: elevated rail, no Vin/VBAT comparison.
        assert!(online_raw(9_088_000, 4_400_000, 1_887_000, false));
        // Elevated rail even with a dead current sample.
        assert!(online_raw(9_000_000, 4_400_000, IIN_FLOOR_UA, false));
    }

    #[test]
    fn unplugged_elevated_rail_is_not_online() {
        // Live measurement 19.09 with the cable unplugged: the VIN node is unloaded
        // and the ADC reads exactly 2 · VBAT. This branch used to return `true`
        // unconditionally, and the tray showed "connected" while the pack discharged.
        assert!(!online_raw(8_800_000, 4_400_000, IIN_FLOOR_UA, false));
        // The reflection scales with the cell: on a discharged cell it lands in
        // 4.2-8.0 V, below the 6 V "raised bus" threshold.
        assert!(!online_raw(7_600_000, 3_800_000, IIN_FLOOR_UA, false));
        assert!(!online_raw(5_000_000, 2_500_000, IIN_FLOOR_UA, false));
        // Current overrides the reflection: 2 A into the pack at 9 V is an adapter, not a bus.
        assert!(online_raw(8_800_000, 4_400_000, 2_000_000, false));
    }

    #[test]
    fn hardware_unplug_flag_is_a_veto_until_current_flows() {
        // `FAULT1` bit 4 is set by the hardware - the adapter is unplugged. The ADC
        // may still show residual bus voltage.
        assert!(!online_raw(9_400_000, 4_400_000, IIN_FLOOR_UA, true));
        // Evidence beats the latch: if current flows into the pack the adapter is
        // there, and a stale bit has no right to drop `POWER_ON_LINE`.
        assert!(online_raw(9_400_000, 4_400_000, 2_000_000, true));
    }

    #[test]
    fn idle_five_volt_adapter_is_online() {
        // Floating 5 V brick, pack full, no current: still AC (VIN > VBAT+200 mV).
        assert!(online_raw(5_000_000, 4_300_000, 0, false));
        // And exactly at the VBAT guard: 4.6 V against a 4.45 V pack is AC.
        assert!(online_raw(4_600_000, 4_300_000, 0, false));
    }

    // --- Input sample validity (ADC hibernation / bus failure) -------------
    //
    // Live list from the 19.09 dump (`AdcValid = 1`): the chip reads successfully
    // but all channels are zeros, because the ADC went to sleep. While such a tick
    // counted as a measurement, `online_raw` answered "no adapter" and the
    // `POWER_ON_LINE` hold aged on every tick (8 s - and the tray gives
    // AC→DC→AC, although the cable never moved).

    /// Unreadable tick: all channels zero, `FAULT1` bit 4 clear - no verdict.
    const TICK_NO_READING_VBUS_PRESENT: Option<bool> =
        online_raw_with_evidence(0, 0, 0, false, false);
    /// Unreadable tick, but the hardware itself said "VAC unplugged".
    const TICK_NO_READING_VBUS_GONE: Option<bool> = online_raw_with_evidence(0, 0, 0, true, false);
    /// Readable tick of real 2:1 (live measurement: 9.088 V, 1.887 A).
    const TICK_READING_ELEVATED: Option<bool> =
        online_raw_with_evidence(9_088_000, 4_400_000, 1_887_000, false, true);

    #[test]
    fn unreadable_tick_carries_no_verdict_when_the_hardware_says_vbus_is_present() {
        assert_eq!(
            TICK_NO_READING_VBUS_PRESENT, None,
            "zeros without a hardware verdict are \"no sample\", not \"no adapter\""
        );
        // The same tick, taken as a measurement, is exactly the old verdict
        // "no adapter"; that is the one that must not feed the hold.
        assert!(!online_raw(0, 0, 0, false));
        // The hardware verdict, however, is accepted even without the ADC.
        assert_eq!(TICK_NO_READING_VBUS_GONE, Some(false));
        // A readable tick goes through the full `online_raw` chain.
        assert_eq!(TICK_READING_ELEVATED, Some(true));
    }

    #[test]
    fn unreadable_run_while_hardware_says_vbus_present_never_clears_online() {
        // 60 s of unreadable ticks is five times `ONLINE_HOLD_MS`: if zeros aged
        // the hold, `POWER_ON_LINE` would go out at second 12.
        let armed_at = 250_u64;
        let mut st = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        while now < armed_at + 60_000 {
            now += TICK_MS;
            st = st.update_evidence(TICK_NO_READING_VBUS_PRESENT, now, ONLINE_HOLD_MS);
        }
        assert!(
            st.held,
            "60 s of unreadable ticks with VAC_UNPLUG clear do not clear POWER_ON_LINE"
        );
        assert_eq!(st.last_true_ms, armed_at, "the evidence mark must not move");
        // Test load: the same sequence of zeros taken as a measurement (the old
        // behaviour) clears the hold.
        let mut as_before = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        while now < armed_at + 9_000 {
            now += TICK_MS;
            as_before = as_before.update(online_raw(0, 0, 0, false), now, ONLINE_HOLD_MS);
        }
        assert!(
            !as_before.held,
            "zeros taken as 0 V clear the hold - that is the old defect"
        );
    }

    #[test]
    fn unreadable_run_cannot_arm_or_rearm_a_cleared_hold() {
        // "Do not know" is not evidence the other way either: a run of unreadable
        // ticks does not arm the hold even when the hardware says nothing about unplug.
        let mut st = Hold::new();
        let mut now = 0_u64;
        while now < 30_000 {
            now += TICK_MS;
            st = st.update_evidence(TICK_NO_READING_VBUS_PRESENT, now, ONLINE_HOLD_MS);
        }
        assert!(
            !st.held,
            "unreadable ticks on their own do not arm POWER_ON_LINE"
        );
        // Live evidence brings the flag back: the first tick starts the run, the
        // second arms it (HOLD_ARM_RUN).
        let first = st.update_evidence(TICK_READING_ELEVATED, now, ONLINE_HOLD_MS);
        assert!(
            !first.held,
            "a lone piece of evidence does not arm the hold"
        );
        now += TICK_MS;
        let armed = first.update_evidence(TICK_READING_ELEVATED, now, ONLINE_HOLD_MS);
        assert!(armed.held, "sustained evidence brings POWER_ON_LINE back");
        // ...and it holds for its full 8 s even if the ADC went back to sleep.
        let armed_at = now;
        let mut held = armed;
        while now < armed_at + 7_750 {
            now += TICK_MS;
            held = held.update_evidence(TICK_NO_READING_VBUS_PRESENT, now, ONLINE_HOLD_MS);
        }
        assert!(
            held.held,
            "evidence that armed in time keeps the window to the end"
        );
    }

    #[test]
    fn genuine_unplug_clears_online_through_both_paths() {
        // An unplug the hardware reported ends the hold both when the ADC sleeps
        // (zeros + bit 4) and when the bus is still readable (a 2·VBAT phantom).
        let armed_at = 250_u64;
        let mut st = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        while now < armed_at + 7_750 {
            now += TICK_MS;
            st = st.update_evidence(TICK_NO_READING_VBUS_GONE, now, ONLINE_HOLD_MS);
        }
        assert!(st.held, "up to 8 s the unplug keeps the window, as before");
        while now < armed_at + 9_000 {
            now += TICK_MS;
            st = st.update_evidence(TICK_NO_READING_VBUS_GONE, now, ONLINE_HOLD_MS);
        }
        assert!(
            !st.held,
            "the VAC_UNPLUG bit clears POWER_ON_LINE even without samples"
        );

        // Readable unplug: the phantom bus and the current at the ADC floor give
        // the same verdict from both branches, so the new one does not alter `online_raw`.
        let readable = online_raw_with_evidence(8_800_000, 4_400_000, IIN_FLOOR_UA, true, true);
        assert_eq!(readable, Some(false));
        assert!(!online_raw(8_800_000, 4_400_000, IIN_FLOOR_UA, true));
        let mut readable_run = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        while now < armed_at + 9_000 {
            now += TICK_MS;
            readable_run = readable_run.update_evidence(
                online_raw_with_evidence(8_800_000, 4_400_000, IIN_FLOOR_UA, true, true),
                now,
                ONLINE_HOLD_MS,
            );
        }
        assert!(!readable_run.held, "a readable unplug clears the hold");
    }
}
