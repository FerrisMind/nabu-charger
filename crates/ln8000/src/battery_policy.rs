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

/// How long a rise of the fuel counter's SOC keeps counting as charge evidence (ms).
///
/// The counter is the platform's own statement about the pack, and its *direction* is
/// what the input node cannot supply: `FG_MONOTONIC_SOC` falls while the pack drains
/// and rises while it fills, measured on 22.09 in both states. Its step is coarse
/// (1/255 of the range, about 0.39 %), so a rise has to be remembered: at 1.5 A one step
/// arrives every ~80 s, and this window is several steps wide.
///
/// A *fall* is not remembered at all - it clears the verdict on the spot, which is what
/// keeps a weak brick honest (a pack draining behind a live input node is exactly the
/// "discharging while the tray says charging" report from 19.09).
pub const SOC_RISE_HOLD_MS: u64 = 300_000;

/// The fuel counter's SOC and whether it is on the way up.
///
/// Deliberately **not** a comparison against a baseline taken `SOC_RISE_HOLD_MS` ago:
/// that would drop the verdict for a whole step every time the window rolled over, and
/// on a charging pack that is up to a minute and a half of "not charging" every five
/// minutes. A rise is what the counter is asked for, and the verdict stands while rises
/// keep arriving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocTrend {
    /// The previous raw SOC sample.
    ///
    /// `u8::MAX` means "nothing seen yet": it is above every legal sample (255 is exactly
    /// 100 %), so the first sample cannot be mistaken for a rise.
    prev_raw: u8,
    /// Whether the last rise is recent enough to count.
    pub rising: bool,
    /// Monotonic time of the last rise, ms (zero - none yet).
    pub last_rise_ms: u64,
}

impl SocTrend {
    /// Nothing seen yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prev_raw: u8::MAX,
            rising: false,
            last_rise_ms: 0,
        }
    }

    /// Applies one raw SOC sample (`FG_MONOTONIC_SOC`, 0...255).
    #[must_use]
    pub const fn update(self, raw: u8, now_ms: u64) -> Self {
        if raw > self.prev_raw {
            return Self {
                prev_raw: raw,
                rising: true,
                last_rise_ms: now_ms,
            };
        }
        if raw < self.prev_raw {
            // The pack is draining (or the counter re-estimated downwards): the verdict
            // ends here rather than lingering for the rest of the window.
            return Self {
                prev_raw: raw,
                rising: false,
                last_rise_ms: self.last_rise_ms,
            };
        }
        Self {
            prev_raw: raw,
            rising: self.rising && now_ms.saturating_sub(self.last_rise_ms) < SOC_RISE_HOLD_MS,
            last_rise_ms: self.last_rise_ms,
        }
    }
}

impl Default for SocTrend {
    fn default() -> Self {
        Self::new()
    }
}

/// Held boolean plus the timestamp of the last raw `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// Last reported (held) value.
    pub held: bool,
    /// Monotonic milliseconds of the last raw `true`.
    pub last_true_ms: u64,
    /// Consecutive raw `true` samples seen so far.
    true_run: u32,
    /// Samples a cold hold needs to arm: [`HOLD_ARM_RUN`], or [`HOLD_FRONT_RUN`]
    /// for a hold built with [`Hold::front_armed`].
    front_run: u32,
}

/// Consecutive raw `true` samples required to **extend** a hold, and to arm one
/// that is not front-armed.
///
/// One lone `true` between `false` samples must not move the window: a raw
/// predicate flapping faster than `hold_ms` would otherwise pin the flag
/// forever without any sustained evidence. With this requirement the flag is
/// bounded — it clears `hold_ms` after the last *run* of [`HOLD_ARM_RUN`]
/// consecutive true samples.
pub const HOLD_ARM_RUN: u32 = 2;

/// Consecutive raw `true` samples required to arm a **front-armed** hold
/// ([`Hold::front_armed`]) — one, because the tick that carries the first `true`
/// is the only sample the driver gets for the next several seconds.
///
/// The HVDCP bring-up runs inside the same telemetry callback that publishes the
/// verdict (`evt_telemetry_timer` negotiates only after the sample is out), and it
/// blocks the timer for seconds. Measured on 23.09 with `20.47.10.672`:
/// `OnlineRaw = 1` at 23:48:00.250 and the next tick at 23:48:09.011 — 8,5 s in
/// which a run-arming rule kept the hold cold with the adapter already attached,
/// so Windows showed AC 8,7 s after the cable. The verdict does not need that
/// second sample: it is a direct ADC measurement of the input, with the
/// doubled-VBUS veto in front of it.
///
/// The price is bounded and known: a hold armed by one sample that no second
/// sample confirms lives exactly `hold_ms` from that sample (a lone `true` never
/// moves `last_true_ms`), so a raw predicate chattering with at least one `true`
/// per window can keep the flag up where a run-armed hold would have dropped it.
/// Only the online verdict pays it: it is a measurement of the adapter, while the
/// charging witness carries history (a window peak and a SOC rise up to
/// [`SOC_RISE_HOLD_MS`] old) and cannot appear before the bring-up anyway.
pub const HOLD_FRONT_RUN: u32 = 1;

impl Hold {
    /// Never-held state, armed by a run of [`HOLD_ARM_RUN`] samples.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: false,
            last_true_ms: 0,
            true_run: 0,
            front_run: HOLD_ARM_RUN,
        }
    }

    /// Never-held state that arms on its **first** raw `true` sample
    /// ([`HOLD_FRONT_RUN`]) instead of waiting for a run. Extension is unchanged:
    /// only a run of [`HOLD_ARM_RUN`] moves the window, so a single sample buys
    /// exactly one window.
    #[must_use]
    pub const fn front_armed() -> Self {
        Self {
            held: false,
            last_true_ms: 0,
            true_run: 0,
            front_run: HOLD_FRONT_RUN,
        }
    }

    /// Applies one sample of the raw predicate.
    ///
    /// `raw` true for the hold's arming run → held, and the timestamp moves to
    /// `now_ms` (a fresh run re-arms the full hold window). A cold hold arms by
    /// [`HOLD_FRONT_RUN`] if it was built with [`Self::front_armed`] and by
    /// [`HOLD_ARM_RUN`] otherwise; an already held one always needs the run.
    /// `raw` false while held → stays held while `now_ms - last_true_ms <
    /// hold_ms`; at `hold_ms` and beyond the hold ends. Timestamps are
    /// monotonic; a non-monotonic `now_ms` keeps the hold (saturating
    /// difference is 0).
    #[must_use]
    pub const fn update(self, raw: bool, now_ms: u64, hold_ms: u64) -> Self {
        if raw {
            let true_run = self.true_run.saturating_add(1);
            // A run extends a window that already exists; a cold hold arms by its
            // own rule, which is one sample for a front-armed hold.
            let needed_run = if self.held {
                HOLD_ARM_RUN
            } else {
                self.front_run
            };
            if true_run >= needed_run {
                Self {
                    held: true,
                    last_true_ms: now_ms,
                    true_run,
                    front_run: self.front_run,
                }
            } else {
                // First sample of a run: not evidence yet, do not extend.
                Self {
                    held: self.held,
                    last_true_ms: self.last_true_ms,
                    true_run,
                    front_run: self.front_run,
                }
            }
        } else if self.held && now_ms.saturating_sub(self.last_true_ms) < hold_ms {
            Self {
                held: self.held,
                last_true_ms: self.last_true_ms,
                true_run: 0,
                front_run: self.front_run,
            }
        } else {
            Self {
                held: false,
                last_true_ms: self.last_true_ms,
                true_run: 0,
                front_run: self.front_run,
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

/// Raw charging predicate before hysteresis: the adapter is held online and either the
/// instantaneous `Iin` or the window peak is at/above [`IIN_CHARGING_UA`], or the fuel
/// counter's SOC is on the way up ([`SocTrend`]).
///
/// The peak matters because a single tick can read the ADC floor while the window as a
/// whole is charging. The SOC trend is the other witness, and it exists because the
/// pump's `Iin` is blind whenever the current is carried by the platform's own buck: on
/// a 5 V brick the live tablet charges at ~1.5 A with `Iin` on the 39 mA ADC floor
/// (measured 22.09), and this predicate would otherwise never be true there.
///
/// The *direction* is why the SOC is the witness and not the counter's cell current: the
/// current field carries a usable magnitude, but its sign was measured **not** to
/// discriminate the two directions on this board (22.09: negative both while the pump
/// pushed 3.5 A into a pack whose SOC climbed, and while the pack drained at 0.54 A with
/// the cable out and the SOC falling). The SOC answered correctly in both states.
#[must_use]
pub const fn charging_raw(
    online_held: bool,
    iin_ua: u32,
    iin_peak_ua: u32,
    soc_rising: bool,
) -> bool {
    online_held && (iin_ua >= IIN_CHARGING_UA || iin_peak_ua >= IIN_CHARGING_UA || soc_rising)
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
/// The 4.6-6.0 V band is the one place where voltage alone cannot answer, because a
/// 5 V brick with the pump out of transfer and a 5 V node with nothing behind it read
/// the same there: measured on 22.09 at 20:34, a brick that really delivers current
/// (the operator's meter showed it) read `Vin = 4 384 000-5 056 000`, a cell at
/// `4 055 000`, `Iin` on the 39 120 uA floor and bit 4 clear in the windows between the
/// driver's own pulses - every field identical to the floating node of 14:57. **Asking
/// the band for current was tried on 22.09 and reverted**: with this brick the pump
/// never reaches its mode (`LastEnableErr = -4`, `ModeNotReached`, twelve attempts), so
/// no current ever flows, the band answered "no adapter" forever and the tray showed
/// nothing while the tablet was charging. The band therefore stays voltage-only and the
/// phantom route through it stays open; see `docs/FINDINGS.md` for why a single tick
/// cannot separate the two cases and what a fix would have to measure instead.
///
/// `vac_unplug`: `FAULT1` bit 4 (`LN8000_MASK_VAC_UNPLUG_STS`, vendor
/// `ln8000_charger.h:62`). The vendor answers **with this bit** the question "is
/// VBUS" (`POWER_SUPPLY_PROP_TI_VBUS_PRESENT` → `!vac_unplug`,
/// `ln8000_charger.c:948`), so unplug comes from the hardware, not from the ADC.
/// Live measurement 19.09 with the cable unplugged: `FAULT1 = 0x30` (bit 4 set),
/// `Vin = 8.80 V` with the cell at `4.40 V`, current 39 mA.
///
/// The bit is a live verdict on the node it reads, so it is only as good as that
/// node: on 22.09 at 20:34 it was set in 290 ms windows every 15-40 s while the
/// driver's own pulses were collapsing a working brick's output. That is why the
/// unplug *release* in the KMDF tick requires the bit to hold for two ticks
/// ([`unplug_release`]) while this function keeps answering on the single sample -
/// the veto here is the old, measured behaviour.
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
    if is_reflection_tick(vbus_uv, vbat_uv, iin_ua, vac_unplug) {
        return false;
    }
    if vbus_uv >= VBUS_ELEVATED_UV {
        return true;
    }
    // Below `VBUS_ELEVATED_UV` only two tests are left: the floor, and the headroom over
    // the cell. There is deliberately **no** 4.6 V floor here any more. Measured on
    // 22.09, a 5 V brick delivering ~1.5 A into the pack through the platform buck held
    // the node at 4.40-4.54 V with the cell at 4.13-4.18 V, so the 4.6 V floor published
    // "on battery" for the whole session while the operator's meter showed the current -
    // and Windows applied the DC idle policy to a tablet sitting on a brick, which is the
    // same failure the floor was introduced to prevent, reached from the other side. The
    // comparator has already spoken above: a node below 4.6 V with bit 4 clear is a
    // loaded adapter, and an unplugged node is not this case - with no cable the
    // reflection is `2 · VBAT` (measured 8.46 V at a 4.23 V cell and 8.86 V at 4.43 V),
    // which the doubling veto catches before this point.
    if vbus_uv < VBUS_CHARGING_MIN_UV {
        return false;
    }
    // Vin must also be clearly above the pack so VBAT float is not read as AC.
    if vbat_uv > 0 && vbus_uv < vbat_uv.saturating_add(VBUS_ABOVE_VBAT_UV) {
        return false;
    }
    true
}

/// True when the tick's `Vin` is the converter's own `2 · VBAT` reflection and
/// nothing above it in the ladder has spoken: no current into the pack, the
/// hardware's VBUS bit clear, the rail at or above [`VBUS_ONLINE_UV`].
///
/// This is the one rejection in [`online_raw`] that also happens **with a live
/// adapter attached**, and that is a measurement rather than a possibility. While
/// the pump is out of transfer its input node is unloaded, so the ADC reads the
/// reflection - which the notes call the pump's normal operating point rather than
/// evidence of absence (`docs/FINDINGS.md`). Live 22.09: AC → DC → AC in 2.647 s
/// with the cable motionless and the pump idle, `FAULT1` bit 4 clear on every row
/// (the hardware's own VBUS detector said the cable was there, so the new
/// `held`-branch was never reachable) and `Microsoft-Windows-Kernel-Power` 105
/// eleven times in twelve seconds - each one a power-source change that re-applies
/// the display policy. That is the reported backlight reset.
///
/// A tick that reads the reflection therefore carries **no verdict**: it is the pump
/// talking about itself, not about the cable. [`online_raw_with_evidence`] answers
/// `None` for it, which leaves the online hold untouched, and only the hardware bit
/// ends the window without a measurement. [`online_raw`] keeps its historical answer
/// (`false`) for the bool callers: on the node alone, a reflection is not an adapter.
///
/// The reflection is also the only route into the 4.6 V gate: a cell below ~2.3 V
/// reflects to less than [`VBUS_ONLINE_UV`], and such a tick is decided by the
/// floor/headroom branch instead - see the open defect in `README.md`.
#[must_use]
pub const fn is_reflection_tick(vbus_uv: u32, vbat_uv: u32, iin_ua: u32, vac_unplug: bool) -> bool {
    // The two guards that sit above the veto in `online_raw`, repeated so that the
    // predicate is complete on its own: current into the pack wins outright, and the
    // hardware's unplug bit is a verdict of its own.
    if vbus_uv >= VBUS_CHARGING_MIN_UV && iin_ua >= IIN_CHARGING_UA {
        return false;
    }
    if vac_unplug {
        return false;
    }
    vbus_uv >= VBUS_ONLINE_UV && vin_is_doubled_vbat(vbat_uv, vbus_uv)
}

/// [`online_raw`] for one tick **together with that tick's authority to age the
/// hold**.
///
/// A zero in this telemetry is not "0 V" — it is "no sample", and it arrives by
/// two different routes:
///
/// * a bus read failure collapses to zero at the call site (`vbat_read
///   .unwrap_or_default()` in the KMDF tick, `ln8000-kmdf/src/lib.rs:2511-2514`);
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
/// * a readable tick that is a [`is_reflection_tick`] → `None`: the node is the
///   pump's own `2 · VBAT`, which is what a *live* adapter looks like while the pump
///   is out of transfer (the measured flap above). No current, no hardware verdict,
///   no verdict from the node either — a cable must not be declared gone on the
///   strength of the pump's own reflection;
/// * not usable and `vac_unplug` set → `Some(false)`: the hardware itself says
///   VBUS is gone (`FAULT1` bit 4, read fresh from the chip in the same tick at
///   `ln8000-kmdf/src/lib.rs:2524`), so the hold must age exactly as before;
/// * not usable and `vac_unplug` clear → `None`: no measurement and no hardware
///   verdict. The caller must **leave the hold untouched**
///   ([`Hold::update_evidence`]) — an unreadable tick is not evidence of absence.
///
/// A real removal still ends the window on the spot, through the bit: measured on
/// 22.09 at 23:49:48.591 the node read its reflection (`8 144 000` against a
/// `4 090 000` cell) with bit 4 clear, and the bit asserted by 23:49:48.874, 283 ms
/// later — [`unplug_release`] then needs three ticks, and the tray went to battery
/// 1.1 s after the cable came out.
///
/// `vac_unplug` is deliberately trusted only in the "not usable" case: while a
/// real `Vin`/`Iin` measurement exists, [`online_raw`] keeps its current
/// precedence (current first, then the bit) and its behaviour is untouched.
///
/// The fuel counter's SOC is deliberately **not** an input here. Its rise is a slow
/// statement (one step per ~80 s at 1.5 A) about the cell, not about the node, so it
/// belongs to the charging predicate, where it can be gated on an already-online adapter
/// ([`charging_raw`]). Presence is decided by what answers on the tick it is read: the
/// node's own readings and the hardware comparator.
#[must_use]
pub const fn online_raw_with_evidence(
    vbus_uv: u32,
    vbat_uv: u32,
    iin_ua: u32,
    vac_unplug: bool,
    input_readings_usable: bool,
) -> Option<bool> {
    if input_readings_usable {
        if is_reflection_tick(vbus_uv, vbat_uv, iin_ua, vac_unplug) {
            // Not "gone": the node is the pump's own reflection while it is out of
            // transfer, which happens with the brick attached (measured flap). The
            // hold must not age on it - the hardware bit is the only witness that
            // ends the window without a measurement.
            return None;
        }
        return Some(online_raw(vbus_uv, vbat_uv, iin_ua, vac_unplug));
    }
    if vac_unplug {
        return Some(false);
    }
    None
}

/// How many consecutive ticks the hardware's "VBUS gone" verdict must hold before
/// the online window is ended on the spot.
///
/// One tick is not enough, and that is a measurement rather than caution: on 22.09 at
/// 20:34, with a working 5 V brick attached, `FAULT1` bit 4 came and went in **290 ms
/// windows every 15-40 s** while the driver's own engagement pulses collapsed the
/// adapter's output. Releasing on the first such tick dropped `POWER_ON_LINE` on every
/// pulse, and with the pump unable to reach its mode (no current, so nothing re-armed
/// the flag from the current branch) the tray stayed dark for the whole session. A real
/// removal is different in exactly this respect: the bit asserted and then stayed set
/// for hours (measured 14:57:30 -> 17:52, and two hours of quiescence before that).
///
/// **Three** rather than two because a 290 ms window is longer than the 250 ms
/// telemetry tick: two samples can fall inside one window, so a run of two can be
/// produced by a pulse alone. Three ticks span 750 ms, which no measured window
/// reaches - and the price is 750 ms on a real removal, against the 7-8 s the window
/// took before this path existed. A pulse longer than 750 ms is still handled: the
/// release is no longer the end of it, because the fuel counter's next sample answers
/// "the pack is taking charge" and re-arms the flag within [`HOLD_ARM_RUN`] ticks.
pub const UNPLUG_RELEASE_RUN: u32 = 3;

/// Whether this tick ends the online window at once, and the new run length.
///
/// Returns `(true, _)` when the hardware says VBUS is gone and no current is flowing
/// into the pack for [`UNPLUG_RELEASE_RUN`] consecutive ticks: the window ends on this
/// tick instead of after `ONLINE_HOLD_MS` (8 s), which is what a removal looks like from
/// the tray. `run` is the caller's counter of consecutive qualifying ticks; a tick that
/// does not qualify resets it to zero, so only a sustained verdict releases.
///
/// The current test is what keeps a stale bit from dropping a pack that is really
/// charging, and the run length is what keeps the driver's own pulses from doing it.
/// The current here is the pump's `Iin` and only it: the fuel counter is deliberately
/// **not** part of this test, because its sample can be up to `GAUGE_POLL_MS` old and a
/// history cannot outvote the live verdict of a removal. What keeps a long pulse from
/// darkening the tray is the re-arming, not this predicate.
#[must_use]
pub const fn unplug_release(vac_unplug: bool, iin_ua: u32, run: u32) -> (bool, u32) {
    if !vac_unplug || iin_ua >= IIN_CHARGING_UA {
        return (false, 0);
    }
    let run = run.saturating_add(1);
    (run >= UNPLUG_RELEASE_RUN, run)
}

/// What a tick should do about the ADC's power mode.
///
/// The LN8000 ADC hibernates on its own: init step 9 leaves `ADC_CTRL` in
/// `AutoHibernate` with the `Sec4` delay, and about four seconds of pump idle are
/// then enough for it to fall asleep (`docs/FINDINGS.md`, the hibernation
/// section). A sleeping ADC is invisible: every channel reads successfully and
/// returns `0x00`, so `Vin` is zero, `sample.input_present` is false and the
/// driver's whole input path - charging, HVDCP, the online verdict - sees "no
/// adapter". Nothing in the driver wakes it: `ADC_CTRL` is written only inside
/// `Pump::configure()`, which runs at device start and on the shutdown-recovery
/// paths, and those paths need a live input sample to be reached at all. So a
/// brick that arrives while the ADC sleeps cannot be seen by anything except the
/// hardware's own VBUS comparator (`FAULT1` bit 4), which keeps working.
///
/// That bit is what this predicate turns into an action:
///
/// * `Some(true)` - the hardware says VBUS is present (`vac_unplug` clear) but the
///   samples are unusable, so the ADC is asleep and the tick must wake it
///   (`ADC_CTRL` bits 5:7 = `Normal`). The next tick then carries real samples.
/// * `Some(false)` - the hardware says VBUS is gone: there is nothing to measure,
///   so the ADC may go back to `AutoHibernate` and save its idle current. This
///   outranks the samples of the same tick: they are the last ones we get.
/// * `None` - the samples are usable and VBUS is present: the ADC is awake and the
///   mode is left alone.
///
/// Waking on the comparator is what makes an insertion visible at all from the
/// sleeping state; letting it sleep again on the comparator is what keeps the
/// awake ADC bounded to the time a source is actually attached.
#[must_use]
pub const fn adc_wake_needed(input_readings_usable: bool, vac_unplug: bool) -> Option<bool> {
    if vac_unplug {
        return Some(false);
    }
    if !input_readings_usable {
        return Some(true);
    }
    None
}

/// Lowest cell voltage the PM8150B fuel gauge is trusted to report (µV).
///
/// Below it the sample is not a cell: the platform cuts off far above it, and a
/// raw zero (register pair read, fuel gauge not measuring) would otherwise look
/// like a deeply discharged pack and drag the whole transfer band down with it.
pub const FG_VBATT_MIN_PLAUSIBLE_UV: u32 = 2_500_000;
/// Highest cell voltage the PM8150B fuel gauge is trusted to report (µV).
///
/// The 16-bit pair tops out at 8,0 V (`65535 × 122,07 µV`), so a saturated /
/// misdecoded register also lands below this; the cell's own hard limit
/// (`bat-ovp` 4,56 V) is the physical reason for the bound.
pub const FG_VBATT_MAX_PLAUSIBLE_UV: u32 = 4_600_000;

/// Whether a fuel-gauge cell-voltage sample may drive the transfer band.
#[must_use]
pub const fn fg_vbatt_plausible(uv: u32) -> bool {
    uv >= FG_VBATT_MIN_PLAUSIBLE_UV && uv <= FG_VBATT_MAX_PLAUSIBLE_UV
}

/// The cell voltage the transfer band, the 2:1 admission gate and the QC3 target
/// all ride on: the fuel gauge when it has a plausible sample, else the pump's
/// own VBAT channel.
///
/// # Why the choice exists
///
/// The band is anchored to the *cell* (`2·Vbat + {200,300,400} mV`), so the
/// anchor has to be a cell measurement. The LN8000's own `Vbat` channel is one
/// only while the pump is idle: in 2:1 it reads the middle of the converter bus
/// (≈ `Vin/2`, see `encoding::VBAT_VIN_HALF_SLACK_UV`) — a number that moves with
/// the very bus the band is supposed to measure. A correction driven by it
/// chases its own target: live 24.09 the driver held `HvdcpTarget = 8,18 V`
/// (that is `2 × 3,94 V` from the rail) while the fuel gauge reported a 3,70 V
/// cell on the same board, i.e. the target sat ~0,5 V above the real band, and
/// the bus was walked 7,6 → 8,9 V with the pump dropping out of 2:1 and the
/// current falling to 0,5 A.
///
/// The PM8150B fuel gauge measures the cell at its own terminal on its own SPMI
/// path and its own ADC (the `0x41A0` pair — the same snapshot the cell current
/// comes from), so its sample does not move with the bus; it is capped to
/// [`FG_VBATT_MIN_PLAUSIBLE_UV`]..[`FG_VBATT_MAX_PLAUSIBLE_UV`] so a failed read
/// cannot move the band either.
#[must_use]
pub const fn anchor_cell_vbat_uv(fg_uv: u32, pump_uv: u32) -> u32 {
    if fg_vbatt_plausible(fg_uv) {
        fg_uv
    } else {
        pump_uv
    }
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
            charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA, false),
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
                charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA, false),
                now,
                CHARGING_HOLD_MS,
            );
        }
        assert!(st.held, "the hold survives up to 20 s");
        // 25 s of sustained low current: cleared.
        while now < armed_at + 25_000 {
            now += TICK_MS;
            st = st.update(
                charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA, false),
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
        assert!(charging_raw(true, IIN_FLOOR_UA, 2_000_000, false));
        let st = arm(true, CHARGING_HOLD_MS);
        assert!(st.held);
        // Both readings on the floor: the raw predicate is false and only the
        // hold keeps CHARGING published.
        assert!(!charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA, false));
        // Offline: a peak current cannot make charging true.
        assert!(!charging_raw(false, 2_000_000, 2_000_000, false));
    }

    #[test]
    fn the_soc_trend_reports_the_direction_the_current_field_cannot() {
        // Live 22.09: while charging (the pump in 2:1 at 8.9 V, ~3.5 A into the pack) the
        // SOC climbed 220 -> 223 -> 233 -> 234 and the counter's current field read
        // negative; with the cable out and the pack draining at 0.54 A (cell 4.231 ->
        // 4.221 V, SOC 234 -> 233) the same field also read negative. The magnitude is
        // usable, the sign is not - so the direction comes from the SOC itself.
        let t = SocTrend::new();
        assert!(!t.rising, "nothing seen yet is not a rise");
        // The first sample is not a rise, whatever it is: the baseline is the previous
        // sample, and there is none.
        let t = t.update(220, 1_000);
        assert!(!t.rising);
        // A rise is remembered for the window.
        let t = t.update(221, 2_000);
        assert!(t.rising);
        assert_eq!(t.last_rise_ms, 2_000);
        // Repeating the same sample keeps the verdict while the rise is recent...
        let t = t.update(221, 2_000 + SOC_RISE_HOLD_MS - 1);
        assert!(t.rising);
        // ...and drops it once the window is out, with no fall in between.
        let t = t.update(221, 2_000 + SOC_RISE_HOLD_MS);
        assert!(!t.rising, "an old rise is not evidence");
        // A fall ends the verdict on the spot: this is what keeps a weak brick honest.
        let t = SocTrend::new().update(234, 1_000);
        let t = t.update(233, 2_000);
        assert!(!t.rising);
        // A rise after the fall starts a new verdict.
        let t = t.update(234, 3_000);
        assert!(t.rising);
        assert_eq!(t.last_rise_ms, 3_000);
        // `255` is exactly 100 %, so the sentinel above it cannot be a real sample and the
        // first one can never be mistaken for a rise.
        let t = SocTrend::new().update(255, 1_000);
        assert!(!t.rising);
    }

    #[test]
    fn a_loaded_five_volt_brick_is_online_again() {
        // The operator's session on 22.09, field by field: Vin 4.416 V, Iin on the
        // 39.12 mA ADC floor because the pump never reached its mode, FAULT1 bit 4 clear,
        // the platform buck charging the pack at ~1.5 A and the meter showing it. The node
        // chain answered "no adapter" for the whole session and the tray stayed dark.
        const LIVE_VIN_UV: u32 = 4_416_000;
        const LIVE_VBAT_UV: u32 = 4_145_000;
        assert!(
            online_raw(LIVE_VIN_UV, LIVE_VBAT_UV, IIN_FLOOR_UA, false),
            "a node 271 mV above the cell with the comparator clear is a loaded adapter"
        );
        // The whole session in one row: the flag is published from this verdict.
        assert_eq!(
            online_raw_with_evidence(LIVE_VIN_UV, LIVE_VBAT_UV, IIN_FLOOR_UA, false, true),
            Some(true)
        );
        // The floor that used to stand here: same row, verdict false, tray dark. The row
        // has to stay below it for this test to be about the floor at all.
        const { assert!(LIVE_VIN_UV < 4_600_000) };
        // The pack itself is still rejected: a node at VBAT float is not an adapter, and
        // neither is one below the 4.2 V floor.
        assert!(!online_raw(LIVE_VBAT_UV, LIVE_VBAT_UV, IIN_FLOOR_UA, false));
        assert!(!online_raw(4_250_000, 4_200_000, IIN_FLOOR_UA, false));
        assert!(!online_raw(4_100_000, 4_100_000, IIN_FLOOR_UA, false));
        // The unplugged node is the doubled reflection (measured 8.46 V at a 4.23 V cell
        // and 8.86 V at 4.43 V), and the veto still catches it.
        assert!(!online_raw(8_464_000, 4_232_000, IIN_FLOOR_UA, false));
        assert!(!online_raw(8_864_000, 4_432_000, IIN_FLOOR_UA, false));
        // The hardware's own "VBUS gone" outranks every voltage in the band.
        assert!(!online_raw(LIVE_VIN_UV, LIVE_VBAT_UV, IIN_FLOOR_UA, true));
        assert_eq!(
            online_raw_with_evidence(0, 0, 0, true, false),
            Some(false),
            "an unreadable tick with the bit set is a removal"
        );
        // The charging flag on that same row comes from the SOC trend, because the pump's
        // current is on the floor and the buck's current is invisible to it.
        assert!(charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA, true));
        assert!(!charging_raw(true, IIN_FLOOR_UA, IIN_FLOOR_UA, false));
        // ...and only while the adapter is online: the trend cannot invent one.
        assert!(!charging_raw(false, IIN_FLOOR_UA, IIN_FLOOR_UA, true));
    }

    #[test]
    fn one_sample_arms_only_the_front_armed_hold() {
        // A lone tick of "evidence" between false samples is not a run: a hold armed
        // by run (the charging one) stays cold.
        let st = Hold::new().update(true, 0, ONLINE_HOLD_MS);
        assert!(!st.held, "a single true does not arm a run-armed hold");
        let st = st.update(false, TICK_MS, ONLINE_HOLD_MS);
        let st = st.update(true, 2 * TICK_MS, ONLINE_HOLD_MS);
        assert!(!st.held, "true/false chatter does not arm a run-armed hold");
        // Two consecutive true samples do arm it.
        let st = st.update(true, 3 * TICK_MS, ONLINE_HOLD_MS);
        assert!(st.held, "two true samples in a row arm the hold");

        // The online verdict arms on the tick that sees the adapter: that tick is the
        // only sample for the next several seconds, because the bring-up runs inside
        // the same callback (HOLD_FRONT_RUN, measured 23.09).
        let st = Hold::front_armed().update(true, 0, ONLINE_HOLD_MS);
        assert!(
            st.held,
            "the online verdict publishes with the cable, not one blocked tick later"
        );
        // That one sample buys exactly one window: a lone true never moves
        // `last_true_ms`, so the flag dies `hold_ms` after it unless a run confirms it.
        let mut st = st.update(false, TICK_MS, ONLINE_HOLD_MS);
        assert!(st.held, "one offline tick does not clear AC");
        let mut now = TICK_MS;
        while now < ONLINE_HOLD_MS - TICK_MS {
            now += TICK_MS;
            st = st.update(false, now, ONLINE_HOLD_MS);
        }
        assert!(st.held, "the window runs to its end");
        now += TICK_MS;
        let st = st.update(false, now, ONLINE_HOLD_MS);
        assert!(
            !st.held,
            "an unconfirmed single sample does not outlive its window"
        );
    }

    #[test]
    fn a_lone_true_never_extends_even_a_front_armed_hold() {
        // True every other tick: never two in a row, so the window must stay where the
        // arming sample put it, and the flag must drop at the end of that one window.
        let armed_at = 0_u64;
        let mut st = Hold::front_armed().update(true, armed_at, ONLINE_HOLD_MS);
        assert!(st.held);
        let mut now = armed_at;
        for i in 0..(ONLINE_HOLD_MS / TICK_MS) {
            now += TICK_MS;
            let raw = i % 2 == 1;
            st = st.update(raw, now, ONLINE_HOLD_MS);
        }
        assert_eq!(
            st.last_true_ms, armed_at,
            "chatter must not move the window, only a run of HOLD_ARM_RUN does"
        );
        now += TICK_MS;
        let st = st.update(false, now, ONLINE_HOLD_MS);
        assert!(
            !st.held,
            "with no confirmed run the hold ends one window after the single sample"
        );
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
    fn an_idle_five_volt_brick_is_online_even_with_no_current() {
        // This test was briefly inverted on 22.09 ("the band needs current, because a
        // floating node reads the same") and the inversion is what made a working brick
        // invisible: at 20:34 a 5 V supply that really delivered current (the operator's
        // meter showed it) read `Vin = 4 384 000-5 056 000` with the cell at `4 055 000`,
        // `Iin` on the 39 120 uA floor and bit 4 clear between the driver's own pulses,
        // while the pump never reached its mode (`ModeNotReached`) and therefore never
        // drew current. With the band asking for current, the tray showed nothing at all
        // for the whole session. Voltage-only is the measured-better answer here.
        assert!(online_raw(5_000_000, 4_300_000, 0, false));
        assert!(online_raw(5_000_000, 4_300_000, IIN_FLOOR_UA, false));
        assert!(online_raw(4_600_000, 4_300_000, IIN_FLOOR_UA, false));
        // Below the 4.6 V floor the band does not answer at all, current or not: that
        // floor is what keeps a sagging rail out, and only current evidence (the first
        // branch) can bring a source at 4.4 V back.
        assert!(!online_raw(4_400_000, 4_300_000, IIN_FLOOR_UA, false));
        assert!(online_raw(4_400_000, 4_300_000, 2_000_000, false));
    }

    #[test]
    fn the_five_volt_band_still_rejects_the_doubled_bus() {
        // The residual: a 5 V node with nothing behind it and a cell reading `Vin/2`
        // (the pump's own converter rail) is still vetoed by the doubling test, which is
        // the one guard in this band that the 22.09 measurement did not touch.
        assert!(!online_raw(5_000_000, 2_500_000, IIN_FLOOR_UA, false));
        assert!(!online_raw(5_056_000, 2_528_000, 0, false));
        // The same node with a credible cell under it is a brick: `4 055 000` against
        // `5 056 000` is not half, and that is the reading the tray must accept.
        assert!(online_raw(5_056_000, 4_055_000, IIN_FLOOR_UA, false));
    }

    #[test]
    fn a_reflection_tick_carries_no_verdict() {
        // The flap of 22.09 in one row: the pump left transfer, the node relaxed to its
        // own `2 · VBAT`, the current sat on the 39 mA floor and the hardware bit stayed
        // clear while the brick was attached. As a bool the row is "not an adapter" -
        // the ladder's historical answer; for the hold it is no evidence either way, and
        // that is what keeps Windows from seeing a power-source change.
        const FLAP_VIN_UV: u32 = 8_464_000;
        const FLAP_VBAT_UV: u32 = 4_232_000;
        assert!(!online_raw(FLAP_VIN_UV, FLAP_VBAT_UV, IIN_FLOOR_UA, false));
        assert_eq!(
            online_raw_with_evidence(FLAP_VIN_UV, FLAP_VBAT_UV, IIN_FLOOR_UA, false, true),
            None,
            "the pump's own reflection is not a verdict about the cable"
        );
        // The hardware bit is the witness that does end the window without a measurement.
        assert_eq!(
            online_raw_with_evidence(FLAP_VIN_UV, FLAP_VBAT_UV, IIN_FLOOR_UA, true, true),
            Some(false),
            "a reflection with the unplug bit set is a removal"
        );
        // Current outranks the reflection in both APIs: 2 A into the pack is an adapter
        // whatever the node looks like.
        assert!(online_raw(FLAP_VIN_UV, FLAP_VBAT_UV, 2_000_000, false));
        assert_eq!(
            online_raw_with_evidence(FLAP_VIN_UV, FLAP_VBAT_UV, 2_000_000, false, true),
            Some(true)
        );
        // A node that is not half the cell is not a reflection and keeps its verdict: the
        // 5 V brick of 22.09 (`4 416 000` against a `4 145 000` cell) reads `Some(true)`.
        assert_eq!(
            online_raw_with_evidence(4_416_000, 4_145_000, IIN_FLOOR_UA, false, true),
            Some(true)
        );
    }

    #[test]
    fn a_sustained_reflection_never_clears_the_online_window() {
        // The operator's bug at low state of charge: while the pump is out of transfer the
        // node reads the reflection for seconds, and on every such tick the old verdict
        // aged the hold until the window expired - Windows saw AC -> DC -> AC and
        // re-applied the display policy (`Kernel-Power` 105, eleven events in twelve
        // seconds on 22.09). A reflection tick carries no verdict, so 30 s of them must
        // leave an armed window exactly as it was.
        let armed_at = 250_u64;
        let mut st = arm(true, ONLINE_HOLD_MS);
        let mut now = armed_at;
        let reflected = online_raw_with_evidence(8_464_000, 4_232_000, IIN_FLOOR_UA, false, true);
        assert_eq!(reflected, None, "the row under test is the reflection");
        while now < armed_at + 30_000 {
            now += TICK_MS;
            st = st.update_evidence(reflected, now, ONLINE_HOLD_MS);
        }
        assert!(st.held, "the pump's reflection does not end the AC window");
        assert_eq!(st.last_true_ms, armed_at, "and does not move its timestamp");
        // The hardware's own verdict still ends it on the spot (`hold_ms = 0`), which is
        // the removal path: the bit asserted 283 ms after the cable of 22.09 came out.
        let released = st.update(false, now, 0);
        assert!(!released.held, "the hardware bit still releases the flag");
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
    fn a_removal_needs_three_ticks_but_the_drivers_own_pulses_never_reach_them() {
        // The 22.09 20:34 pulse pattern: bit 4 set for 290 ms while the driver collapsed
        // a working brick's output, then clear again. A 290 ms window is longer than the
        // 250 ms telemetry tick, so one window can hold two samples: three consecutive
        // ticks are the first run a single window cannot produce, and 750 ms is what a
        // real removal costs here (against the 7-8 s the plain window took before this
        // path existed).
        let (release, run) = unplug_release(true, IIN_FLOOR_UA, 0);
        assert!(!release, "the first tick of a verdict is not a removal");
        assert_eq!(run, 1);
        let (release, run) = unplug_release(false, IIN_FLOOR_UA, run);
        assert!(!release);
        assert_eq!(run, 0, "a clear bit resets the run");
        let (release, run) = unplug_release(true, IIN_FLOOR_UA, 0);
        assert!(!release, "one 290 ms window does not release");
        let (release, run) = unplug_release(true, IIN_FLOOR_UA, run);
        assert!(!release, "nor does a second window's worth of ticks");
        assert_eq!(run, 2);
        // A real removal: the bit asserts and stays asserted (measured 14:57:30 ->
        // 17:52 with the node quiescent), so the third tick ends the window.
        let (release, run) = unplug_release(true, IIN_FLOOR_UA, run);
        assert!(release, "the third consecutive tick ends the online window");
        assert_eq!(run, 3);
        // Current into the pack always wins: a stale bit cannot drop a charging pack.
        let (release, run) = unplug_release(true, 2_000_000, 5);
        assert!(!release);
        assert_eq!(run, 0, "a tick with current resets the run");
    }

    #[test]
    fn the_hardware_comparator_wakes_a_sleeping_adc_and_puts_it_back_to_sleep() {
        // Asleep with VBUS present (the brick arrived while the ADC was in
        // `AutoHibernate`): the samples are unusable and bit 4 is clear, so the tick
        // has to wake the chip - otherwise nothing on this platform can see the
        // brick at all, because `input_present`, HVDCP and the engage decision all
        // read `Vin`, and a sleeping ADC answers zero.
        assert_eq!(adc_wake_needed(false, false), Some(true));
        // Asleep with VBUS gone: there is nothing to measure, so it may sleep on.
        assert_eq!(adc_wake_needed(false, true), Some(false));
        // Awake with the source still there: nothing to do, the mode is left alone.
        assert_eq!(adc_wake_needed(true, false), None);
        // The comparator's "VBUS is gone" outranks the samples of that same tick:
        // they are the last readings we will get, and the chip may go back to sleep
        // at once - that is the idle current back, from the pull tick itself.
        assert_eq!(adc_wake_needed(true, true), Some(false));
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

    #[test]
    fn hardware_unplug_with_no_current_ends_the_window_on_that_tick() {
        // What the KMDF tick does when `FAULT1` bit 4 is set and `Iin` is below the
        // charging floor: `update(false, now, 0)` rather than letting the 8 s window
        // run. Measured on 22.09, the removal was visible in the telemetry in under a
        // second and the OS heard about it 7-8 s later - the whole delay was the
        // window, so the window is what this idiom removes.
        let armed = arm(true, ONLINE_HOLD_MS);
        assert!(armed.held);
        let released = armed.update(false, 1_000, 0);
        assert!(
            !released.held,
            "the unplug ends the window on the tick it is seen, not 8 s later"
        );
        // And the arming run restarts, so the next two true samples are required
        // again before the flag returns.
        assert!(!released.update(true, 1_250, ONLINE_HOLD_MS).held);
        assert!(
            released
                .update(true, 1_250, ONLINE_HOLD_MS)
                .update(true, 1_500, ONLINE_HOLD_MS)
                .held
        );
    }

    #[test]
    fn cell_anchor_prefers_the_gauge_and_bounds_it() {
        // Live 24.09: the gauge said 3704 mV while the pump channel said 3695 mV
        // in standby and 3940 mV in 2:1 - the gauge is the one that does not move
        // with the bus, so it wins whenever it is plausible.
        assert_eq!(anchor_cell_vbat_uv(3_704_000, 3_940_000), 3_704_000);
        assert_eq!(anchor_cell_vbat_uv(3_704_000, 3_695_000), 3_704_000);
        // The `0xFFFFFFFF` failure sentinel and a raw zero are not a cell and
        // must not move the band down (or up).
        assert_eq!(anchor_cell_vbat_uv(u32::MAX, 3_700_000), 3_700_000);
        assert_eq!(anchor_cell_vbat_uv(0, 3_700_000), 3_700_000);
        // Below the platform cutoff / above `bat-ovp` the sample is refused.
        assert_eq!(anchor_cell_vbat_uv(2_499_999, 3_700_000), 3_700_000);
        assert_eq!(anchor_cell_vbat_uv(4_600_001, 3_700_000), 3_700_000);
        assert_eq!(anchor_cell_vbat_uv(4_600_000, 3_700_000), 4_600_000);
        assert_eq!(anchor_cell_vbat_uv(2_500_000, 3_700_000), 2_500_000);
        // With no gauge sample at all the pump channel is still the fallback.
        assert_eq!(anchor_cell_vbat_uv(0, 0), 0);
        assert!(fg_vbatt_plausible(3_700_000));
        assert!(!fg_vbatt_plausible(u32::MAX));
    }
}
