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
#[must_use]
pub const fn online_raw(vbus_uv: u32, vbat_uv: u32, iin_ua: u32) -> bool {
    if vbus_uv >= VBUS_ELEVATED_UV {
        return true;
    }
    if vbus_uv >= VBUS_CHARGING_MIN_UV && iin_ua >= IIN_CHARGING_UA {
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
        assert!(st.held, "один такт на полу АЦП не снимает CHARGING");
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
        assert!(st.held, "до 20 с удержание держится");
        // 25 s of sustained low current: cleared.
        while now < armed_at + 25_000 {
            now += TICK_MS;
            st = st.update(
                charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA),
                now,
                CHARGING_HOLD_MS,
            );
        }
        assert!(!st.held, "25 с низкого тока снимают CHARGING");
    }

    #[test]
    fn one_offline_tick_keeps_ac_online() {
        let st = arm(true, ONLINE_HOLD_MS);
        assert!(st.held);
        // One tick below the Vin threshold: online stays held, so battery.rs
        // (DISCHARGING = !online_held) never publishes DISCHARGING.
        let st = st.update(false, 500, ONLINE_HOLD_MS);
        assert!(st.held, "один такт offline не должен давать DISCHARGING");
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
        assert!(st.held, "до 8 с удержание держится");
        while now < armed_at + 9_000 {
            now += TICK_MS;
            st = st.update(false, now, ONLINE_HOLD_MS);
        }
        assert!(!st.held, "9 с offline снимают online_held");
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
        assert!(!st.held, "одиночный true не взводит удержание");
        let st = st.update(false, TICK_MS, ONLINE_HOLD_MS);
        let st = st.update(true, 2 * TICK_MS, ONLINE_HOLD_MS);
        assert!(!st.held, "дребезг true/false не взводит удержание");
        // Two consecutive true samples do arm it.
        let st = st.update(true, 3 * TICK_MS, ONLINE_HOLD_MS);
        assert!(st.held, "два подряд true взводят удержание");
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
        assert!(st.held, "дребезг внутри окна не снимает удержание досрочно");
        while now <= armed_at + ONLINE_HOLD_MS {
            now += TICK_MS;
            st = st.update(false, now, ONLINE_HOLD_MS);
        }
        assert!(
            !st.held,
            "после последнего устойчивого запуска удержание обязано кончиться"
        );
    }

    #[test]
    fn sagging_five_volt_rail_with_current_is_online() {
        // Live 18.09 acceptance: 2.06 A into the pack at Vin 4.384 V. The old
        // 4.6 V floor called this "on battery" while it charged.
        assert!(online_raw(4_384_000, 4_300_000, 2_063_580));
        // Same rail, current tapered to the ADC floor: back to the Vin rules.
        assert!(!online_raw(4_384_000, 4_300_000, IIN_FLOOR_UA));
        // A dead bus cannot deliver current.
        assert!(!online_raw(0, 4_300_000, 2_000_000));
        // Below the bypass floor: not an adapter even with a suspicious sample.
        assert!(!online_raw(3_900_000, 3_800_000, 2_000_000));
    }

    #[test]
    fn unplugged_bypass_is_not_online() {
        // Unplugged, LN8000 Vin tracks VBAT in bypass and input reads the floor.
        assert!(!online_raw(4_250_000, 4_250_000, IIN_FLOOR_UA));
        assert!(!online_raw(4_384_000, 4_380_000, IIN_FLOOR_UA));
    }

    #[test]
    fn elevated_rail_is_always_online() {
        // 2:1 charge-pump band, 1.9 A: elevated rail, no Vin/VBAT comparison.
        assert!(online_raw(9_088_000, 4_400_000, 1_887_000));
        // Elevated rail even with a dead current sample.
        assert!(online_raw(9_000_000, 4_400_000, IIN_FLOOR_UA));
    }

    #[test]
    fn idle_five_volt_adapter_is_online() {
        // Floating 5 V brick, pack full, no current: still AC (VIN > VBAT+200 mV).
        assert!(online_raw(5_000_000, 4_300_000, 0));
        // And exactly at the VBAT guard: 4.6 V against a 4.45 V pack is AC.
        assert!(online_raw(4_600_000, 4_300_000, 0));
    }
}
