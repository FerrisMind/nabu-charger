//! Encoding and decoding of LN8000 values.
//!
//! Repeats the functions of the `ln8000_charger.c` driver so that under Windows
//! values are encoded the same way as under Android.

use crate::error::PumpError;
use crate::regs;

/// Charge pump operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpMode {
    /// State is undefined.
    Unknown,
    /// Idle: switches off, no current flows.
    Standby,
    /// 1:1 mode - the input goes straight to the battery (5 V).
    Bypass,
    /// 2:1 mode - step-down converter (9 V → 4.5 V).
    Switching,
}

impl OpMode {
    /// Mode code, as in `enum ln8000_opmode_` from the driver.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0x0,
            Self::Standby => 0x1,
            Self::Bypass => 0x2,
            Self::Switching => 0x3,
        }
    }

    /// Name for the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Standby => "STANDBY",
            Self::Bypass => "BYPASS",
            Self::Switching => "SWITCHING",
        }
    }

    /// Parses the mode from the `SYS_STS` register value.
    #[must_use]
    pub const fn from_sys_sts(sys_sts: u8) -> Self {
        if sys_sts & (regs::SYS_STS_SHUTDOWN | regs::SYS_STS_STANDBY) != 0 {
            Self::Standby
        } else if sys_sts & regs::SYS_STS_SWITCHING_ENABLED != 0 {
            Self::Switching
        } else if sys_sts & regs::SYS_STS_BYPASS_ENABLED != 0 {
            Self::Bypass
        } else {
            Self::Unknown
        }
    }

    /// The `SYS_CTRL` portion value for switching to this mode.
    ///
    /// The mask is always the same: `STANDBY_EN | EN_1TO1` (see `ln8000_change_opmode`).
    ///
    /// # Errors
    ///
    /// [`PumpError::OutOfRange`] if [`OpMode::Unknown`] cannot be switched to.
    pub const fn sys_ctrl_bits(self) -> Result<u8, PumpError> {
        match self {
            Self::Standby => Ok(regs::SYS_CTRL_STANDBY_EN),
            Self::Bypass => Ok(regs::SYS_CTRL_EN_1TO1),
            Self::Switching => Ok(0),
            Self::Unknown => Err(PumpError::OutOfRange {
                field: "op_mode",
                requested: 0,
            }),
        }
    }

    /// Mask of the `SYS_CTRL` bits this mode controls.
    #[must_use]
    pub const fn sys_ctrl_mask() -> u8 {
        regs::SYS_CTRL_STANDBY_EN | regs::SYS_CTRL_EN_1TO1
    }
}

/// Absolute minimum pump input for 2:1 switching (µV).
///
/// **Not** the admission gate — that is [`min_vin_for_switching_uv`]
/// (`2 * Vbat + 250 mV`, Android `cp_qc30.c` `VBUS_COMP`, with
/// [`ELEVATED_MIN_VIN_UV`] as the absolute floor). This constant stays as the
/// boundary of the vendor 9 V-class bus: it is what the `FORCE_9V` wait treats
/// as "the brick answered", and what the driver uses for a *sticky* 2:1 verdict
/// while the pump already runs (a sag under load must not downgrade a working
/// transfer).
///
/// It is deliberately no longer the admission floor. Live 24.09 (cell 3,70 V,
/// `FgVbattMv` 3704): the band for that pack is [7,60; 7,80] V, i.e. entirely
/// *below* this 8 V line, so a gate pinned here admitted nothing inside the
/// band — the driver held the bus at 8,0–9,0 V (above the band top, where the
/// pump carries the 39 mA floor) and any DEC batch that landed at 7,6–7,9 V fell
/// through to the 1:1 bypass instead: 2,7 A with 2,9 V across the pass FETs, the
/// loaded bus sagging to 6,7 V, and a mode flap switching ↔ bypass ↔ standby
/// every few seconds.
pub const SWITCHING_MIN_VIN_UV: i32 = 8_000_000;

/// Bus at or above this is no longer a ~5 V input (µV).
///
/// Above it 1:1 is forbidden (the pass FETs would carry `Vin − Vbat`, live 2,9 V
/// at 2,7 A) and 2:1 is admitted from the pack-relative gate alone. Below it the
/// 5 V high-current bypass is the right path for a DCP-class brick.
///
/// 6,0 V is the same boundary the HVDCP retreat test uses for "the brick stayed
/// near 5 V" (a 5 V brick sags to 4,45–5,05 V under load; 6 V is above anything
/// a 5 V source reaches), and the same one the bus policy uses to decide that a
/// 5 V-state correction is no longer silencable (see `FIVE_V_STAY_MAX_UV`).
pub const ELEVATED_MIN_VIN_UV: i32 = 6_000_000;

/// Headroom above `2 * Vbat` required to enter 2:1 (µV).
///
/// Android `cp_qc30.c` `VBUS_COMP` = 250 mV in the `CONFIG_CHARGER_LN8000`
/// build: the charge pump cracks only when `Vbus - 2*Vbat` has real margin.
pub const SWITCHING_HEADROOM_UV: u32 = 250_000;

/// Minimum Vin that can physically feed 2:1 at `vbat_uv` (µV).
///
/// The pack-relative half is Android's gate; the absolute half is
/// [`ELEVATED_MIN_VIN_UV`], below which the bus is a 5 V input and 2:1 is never
/// requested. The absolute floor used to be [`SWITCHING_MIN_VIN_UV`] (8,0 V),
/// which for any cell under ~3,875 V sits *above* the transfer band top
/// (`2*Vbat + 400 mV`) — a state in which the pump reports mode 3 and carries the
/// 39 mA floor while the driver keeps pulsing. Live 24.09 with the pack at
/// 3,68–3,70 V: gate 8,0 V, band [7,56; 7,80] V, bus walked between 6,7 V and
/// 9,0 V with the transfer in 2:1 barely half the time.
///
/// Live nabu case (PD brick): 8.416 V with the pack at 4.275 V cannot run 2:1
/// (needs ≥ 2·4.275 + 0.25 = 8.8 V) — `enable_switching` then returns
/// `ModeNotReached` every tick. Exit to standby instead of retrying.
#[must_use]
pub const fn min_vin_for_switching_uv(vbat_uv: u32) -> u32 {
    let pack_gate = vbat_uv
        .saturating_mul(2)
        .saturating_add(SWITCHING_HEADROOM_UV);
    if pack_gate < ELEVATED_MIN_VIN_UV as u32 {
        ELEVATED_MIN_VIN_UV as u32
    } else {
        pack_gate
    }
}

/// Top of the 2:1 transfer band above `2 * Vbat` (µV) — 400 mV.
///
/// Live nabu 18.09 (pack 4.40–4.44 V): the LN8000 moves real power only while
/// `Vin` sits between roughly `2*Vbat + 200 mV` and `2*Vbat + 400 mV`. Above the
/// top the pump still reports mode 3 (`SYS_STS = 0x04`) but carries only the
/// 39 mA ADC floor (8 × 4.89 mA) with `FAULT2_IIN_OC` flapping 3↔1; below the
/// floor the mode is refused outright. Four measured points bracket the band at
/// Vbat 4.40–4.44 V: 9088 mV → 1887 mA and 9280 mV → 2513 mA, against 9744 mV
/// → 39 mA and 9888 mV → 39 mA. The window rides with the pack: at 4.42 V its
/// top sits at 9.24 V, three QC3 steps *below* the 9.5 V this driver held.
pub const SWITCHING_WINDOW_TOP_UV: u32 = 400_000;

/// Bottom of the *empirical* transfer band above `2 * Vbat` (µV) — 200 mV.
///
/// Kept separate from [`SWITCHING_HEADROOM_UV`] (250 mV, the Android admission
/// gate): the winning live point sat 218 mV above `2*Vbat` and still carried
/// 1887 mA, so admission keeps the vendor gate while the bus policy aims inside
/// the measured band.
pub const SWITCHING_WINDOW_FLOOR_UV: u32 = 200_000;

/// Where to aim inside the band, above `2 * Vbat` (µV) — the middle, 300 mV.
///
/// One QC3 step is 200 mV, i.e. wider than the 200 mV band itself, so aiming at
/// an edge means a single pulse leaves the band — and leaving it *upward* stops
/// the transfer entirely. The centre keeps ~100 mV of margin on both sides.
pub const SWITCHING_WINDOW_TARGET_UV: u32 = 300_000;

/// Top of the transfer band for `vbat_uv` (µV).
#[must_use]
pub const fn window_top_uv(vbat_uv: u32) -> u32 {
    vbat_uv
        .saturating_mul(2)
        .saturating_add(SWITCHING_WINDOW_TOP_UV)
}

/// Bottom of the transfer band for `vbat_uv` (µV).
#[must_use]
pub const fn window_floor_uv(vbat_uv: u32) -> u32 {
    vbat_uv
        .saturating_mul(2)
        .saturating_add(SWITCHING_WINDOW_FLOOR_UV)
}

/// Bus target inside the band for `vbat_uv` (µV).
#[must_use]
pub const fn window_target_uv(vbat_uv: u32) -> u32 {
    vbat_uv
        .saturating_mul(2)
        .saturating_add(SWITCHING_WINDOW_TARGET_UV)
}

/// True when `vin_uv` is inside the transfer band for `vbat_uv`.
///
/// The band is 200 mV wide while one QC3 pulse moves the bus 200 mV, so sitting
/// outside it is a normal intermediate state, not a fault: the caller corrects
/// with one pulse. `false` for a non-positive Vin or an unknown pack.
#[must_use]
pub const fn vin_in_switching_window(vin_uv: i32, vbat_uv: u32) -> bool {
    if vin_uv <= 0 || vbat_uv == 0 {
        return false;
    }
    // The guard above proved `vin_uv > 0`, so the magnitude is the same value and the
    // band can be compared in unsigned space without a wrapping cast.
    let vin = vin_uv.unsigned_abs();
    vin >= window_floor_uv(vbat_uv) && vin <= window_top_uv(vbat_uv)
}

/// Non-negative Vin in µV for unsigned comparisons (`0` for absent / negative).
#[must_use]
pub const fn non_negative_uv(vin_uv: i32) -> u32 {
    if vin_uv > 0 { vin_uv.unsigned_abs() } else { 0 }
}

/// IIN above which a mode-3 pump counts as *carrying power* (µA).
///
/// The bus correction ([`should_walk_window`]) reads this against the *window
/// peak*: 0,3 A sits far above the LN8000 `Iin` ADC floor (39 mA — 8 × 4,89 mA,
/// the pump's zero) and below every measured working point — 0,75–2,86 A live in
/// 2:1, 2,7 A in 1:1 on 24.09. A single instantaneous sample dips to the floor
/// between the pump's own current pulses, so the instantaneous value is not a
/// witness of "no transfer"; the peak over the window is.
pub const IIN_USEFUL_UA: u32 = 300_000;

/// Whether a mode-3 pump carries useful power (window peak above [`IIN_USEFUL_UA`]).
#[must_use]
pub const fn transfer_is_useful(max_iin_ua: u32) -> bool {
    max_iin_ua > IIN_USEFUL_UA
}

/// Whether the tick may move the bus with a QC3 pulse batch.
///
/// The band is the *loaded* bus: a live 2:1 pulls it down by the sag of the brick
/// and the cable, so "below the band" during a transfer is the load's signature,
/// not a fault, and one INC step is what recovers the current. Live 24.09 the
/// driver ran 1,7 A with the loaded bus inside its band; with no correction at
/// all a 3,884 V cell sat at 7,90 V loaded (band floor 7,97 V) and carried
/// 0,8 A, against 1,887 A measured at 218 mV of overdrive.
///
/// Pulsing *down* on a live transfer is the case this gate exists for: the old
/// band was anchored to the pump's own `Vbat` channel, which during 2:1 reads the
/// converter rail (≈ `Vin/2`, see [`VBAT_VIN_HALF_SLACK_UV`]) - a number that
/// moves with the very bus the band is supposed to measure. The correction then
/// chased its own target: live 24.09 it held `HvdcpTarget = 8,18 V` (`2 × 3,94 V`
/// from the rail) against a 3,704 V cell, i.e. a target ~0,5 V above the real
/// band, and the batches walked the bus 7,6 → 8,9 V with the pump dropping out of
/// 2:1 and the current falling 2,2 A → 0,5 A every round (`NudgeInc` 6 /
/// `BoostInc` 6 with `WindowDead = 0`). An anchor that mirrors the bus must never
/// drive it down; a transfer that is carrying current is its own evidence about
/// where the band is.
///
/// * `dead` - mode 3 on the 39 mA floor (window peak included): the bus is in the
///   wrong place, correct it in either direction;
/// * `below_band` - the loaded bus is under the floor: raise it, transfer or not;
/// * otherwise a live transfer is left alone.
#[must_use]
pub const fn should_walk_window(dead: bool, transferring: bool, below_band: bool) -> bool {
    dead || !transferring || below_band
}

/// Whether a tick that found no admissible mode may stop a running charge.
///
/// `admissible` is `charge_mode(..).is_some()` - the answer to "may 2:1 start on
/// this bus?", measured on the bus as it is *right now*. A live 2:1 is the load
/// on that very bus and lifts the cell with its own current, so the loaded sample
/// comes out below the admission gate: live 24.09, 3,875 V relaxed cell (8,00 V
/// gate) with the bus at 8,11 V idle, while the same bus under 0,9 A of transfer
/// read 7,80–7,95 V, i.e. `desired = None` on every loaded tick (`EngageState = 4`
/// in the driver marks, `SuMode = 3` at the same moment).
///
/// Stopping on such a sample is what produced the alternation the operator sees
/// (mode 3 at ~0,9 A ↔ mode 1 at the 39 mA floor, `ChargeAttemptN` +1 per round):
/// stop → the load is gone → the bus relaxes above the gate → 2:1 again → the
/// next loaded sample is below it → stop. A transfer that is carrying current is
/// therefore never stopped by the sampled verdict; the conditions that own a
/// running charge are the guard (current, temperature, taper), not this gate.
#[must_use]
pub const fn stop_running_charge(admissible: bool, transferring: bool) -> bool {
    !admissible && !transferring
}

/// Minimum Vin to attempt any charge mode (µV).
///
/// Live TA200/AFC-class 5 V bricks sag to ~4.45 V under ~2 A bypass load while
/// still charging usefully. The old 4.60 V floor made `SET_CHARGE` return
/// `ModeNotReached` even when direct bypass (`SYS_CTRL=0x01`) already worked.
pub const CHARGE_MIN_VIN_UV: i32 = 4_200_000;

/// Nabu DTS `mi,qc3-bat-volt-max` — QC / taper voltage limit (µV).
pub const NABU_QC3_BAT_VOLT_MAX_UV: u32 = 4_420_000;

/// Nabu DTBO `qcom,non-fcc-fv-max-uv` = 4450 mV — non-FFC charge target.
///
/// This is the pack voltage the non-fast-charge profile is allowed to reach;
/// 4420 mV (`mi,qc3-bat-volt-max`) is only the QC3 loop limit, not a target.
pub const NABU_VBAT_NON_FFC_UV: u32 = 4_450_000;

/// Nabu DTS `ln8000_charger,bat-ovp-threshold` (4560 mV).
pub const NABU_BAT_OVP_UV: u32 = 4_560_000;

/// Android `ln8000_init_device` float: `bat_ovp_th * 100 / 102` → 4470 mV.
/// Hardware `VBAT_OV` ≈ float × 1.02 (= `bat_ovp`). Do not use 4.42 V here —
/// that is the QC loop limit, not `V_FLOAT_CTRL`.
pub const NABU_VBAT_FLOAT_UV: u32 = 4_470_000;

/// Soft headroom above measured Vbat when clearing a near-float OV latch.
pub const VBAT_FLOAT_SOFT_HEADROOM_UV: u32 = 80_000;

/// Cap for soft float raise (recovery soak used ~4.50 V; below `bat_ovp`).
pub const VBAT_FLOAT_SOFT_MAX_UV: u32 = 4_500_000;

/// Near-float taper band (Android: `bat_volt_lmt − 100` mV).
pub const VBAT_TAPER_MARGIN_UV: u32 = 100_000;

/// Tapered LN8000 IIN near float (µA) — useful charge without peak current.
pub const VBAT_TAPER_IIN_UA: u32 = 1_200_000;

/// Computes a soft `V_FLOAT` raise when Vbat is near / above the profile float
/// or `FAULT1_VBAT_OV` is latched. Never exceeds [`VBAT_FLOAT_SOFT_MAX_UV`].
#[must_use]
pub const fn soft_float_for_vbat(profile_float_uv: u32, vbat_uv: u32) -> u32 {
    let with_headroom = vbat_uv.saturating_add(VBAT_FLOAT_SOFT_HEADROOM_UV);
    let raised = if with_headroom > profile_float_uv {
        with_headroom
    } else {
        profile_float_uv
    };
    if raised > VBAT_FLOAT_SOFT_MAX_UV {
        VBAT_FLOAT_SOFT_MAX_UV
    } else {
        raised
    }
}

/// Slack for detecting VBAT ADC riding the 2:1 mid-rail (`≈ Vin/2`).
///
/// Live nabu: with VFLOAT disabled, switching + tiny Iin makes LN8000 VBAT
/// report ~Vin/2 (e.g. 4780 mV @ Vin 9616 mV) while standby reads the pack
/// (~4470 mV). Treat that as converter ceiling, not cell float.
pub const VBAT_VIN_HALF_SLACK_UV: u32 = 80_000;

/// How much Vin must change for the POR budget to count as a new input.
///
/// The 200 mV margin is chosen from the live spread: the same 5 V adapter gives
/// 4.98-5.05 V (within the margin, so POR is not repeated), while a 5 V → 9 V step
/// (QC3/PD) changes the input by volts and opens a new budget.
pub const POR_VIN_TOLERANCE_UV: u32 = 200_000;

/// True when `Vin` is the converter's own `2 · VBAT` reflection, not an adapter.
///
/// The difference from [`vbat_tracks_converter_rail`]: no 8 V threshold - the
/// reflection scales with the cell, and on a discharged cell (2.1-4.0 V) it
/// lands in 4.2-8.0 V, exactly the band where `vbat_tracks_converter_rail` stays
/// silent while [`crate::battery_policy::online_raw`] decides `POWER_ON_LINE`.
///
/// Live case 19.09: cable unplugged, `Vin = 8 800 000`, `VBAT = 4 400 000`
/// (exactly half), current at the ADC floor of 39 mA, Standby mode - and yet the
/// tray reported "connected" while the pack was discharging.
#[must_use]
pub const fn vin_is_doubled_vbat(vbat_uv: u32, vin_uv: u32) -> bool {
    if vbat_uv == 0 {
        return false;
    }
    let half = vin_uv / 2;
    let lo = half.saturating_sub(VBAT_VIN_HALF_SLACK_UV);
    let hi = half.saturating_add(VBAT_VIN_HALF_SLACK_UV);
    vbat_uv >= lo && vbat_uv <= hi
}

/// True when VBAT tracks `Vin/2` (switch-cap rail), not a credible pack reading.
#[must_use]
pub const fn vbat_tracks_converter_rail(vbat_uv: u32, vin_uv: u32) -> bool {
    if vin_uv < SWITCHING_MIN_VIN_UV as u32 {
        return false;
    }
    vin_is_doubled_vbat(vbat_uv, vin_uv)
}

/// Whether a VBAT sample is fit for current decisions.
///
/// Zero volts on a live cell is impossible, but the LN8000 ADC enters auto-
/// hibernation after 4 s of idle: initialisation step 9 writes `ADC_CTRL` bits
/// 5:7 = `AutoHibernate` with the `Sec4` delay, after which the `ADC01..ADC09`
/// registers read **successfully** but hold `0x00` in every channel. Hence
/// `is_ok()` means "the register answered", not "the channel is alive", and zero is invalid.
///
/// Live measurement 19.09 (cable unplugged, `ADC_CTRL = 0x1C` = AutoHibernate):
/// `vbat = 0`, `AdcValid = 0` - that is, the mark reported "all channels read"
/// while the [`crate::guard`] watchdog made current decisions from a non-existent voltage.
#[must_use]
pub const fn vbat_reading_usable(read_ok: bool, vbat_uv: u32) -> bool {
    read_ok && vbat_uv > 0
}

/// True when Vbat is in the Android taper / OV-risk band relative to float.
///
/// Pass `vin_uv` when known: if VBAT is glued to `Vin/2`, this returns `false`
/// unless `FAULT1_VBAT_OV` is latched (real hardware OV still wins).
#[must_use]
pub const fn vbat_near_float(vbat_uv: u32, float_uv: u32, vbat_ov_latched: bool) -> bool {
    vbat_near_float_with_vin(vbat_uv, float_uv, vbat_ov_latched, 0)
}

/// [`vbat_near_float`] with optional Vin for converter-rail rejection.
///
/// The taper band is measured from **the configured charge target** (`float_uv`),
/// not from the QC3 loop limit: with the nabu targets (4.45/4.47 V) the old
/// 4420-mV anchor started cutting current at 4.32 V, which is exactly why the
/// Windows charge looked slower than Android. [`NABU_QC3_BAT_VOLT_MAX_UV`] stays
/// where it belongs — in the QC3 request logic.
#[must_use]
pub const fn vbat_near_float_with_vin(
    vbat_uv: u32,
    float_uv: u32,
    vbat_ov_latched: bool,
    vin_uv: u32,
) -> bool {
    if vbat_ov_latched {
        return true;
    }
    if vin_uv != 0 && vbat_tracks_converter_rail(vbat_uv, vin_uv) {
        return false;
    }
    vbat_uv >= float_uv.saturating_sub(VBAT_TAPER_MARGIN_UV)
}

/// Picks the charge-pump mode from measured Vin **and** Vbat (Android-style).
///
/// * `Vin >= 2*Vbat + 250 mV` (with [`ELEVATED_MIN_VIN_UV`] as the absolute
///   floor) → 2:1
/// * `CHARGE_MIN_VIN_UV .. ELEVATED_MIN_VIN_UV` → 1:1 bypass (5 V path)
/// * elevated but no headroom, or Vin too low → `None` (standby)
///
/// The bypass is **never** selected above [`ELEVATED_MIN_VIN_UV`]: 1:1 feeds the
/// input straight to the pack, so a raised Vin there is both a battery
/// overvoltage risk and, short of it, `Vin − Vbat` burned in the pass FETs. The
/// old upper bound ([`SWITCHING_MIN_VIN_UV`], 8,0 V) left a 6–8 V hole in which
/// the driver itself kept landing after a DEC overshoot, and the 1:1 it then
/// selected sagged the bus to 6,7 V at 2,7 A — the flap described in
/// [`min_vin_for_switching_uv`]. Above 6 V the honest answer for a bus with no
/// 2:1 headroom is standby: the caller's bus correction owns that state.
#[must_use]
pub const fn charge_mode(vin_uv: i32, vbat_uv: u32) -> Option<OpMode> {
    let headroom_ok = non_negative_uv(vin_uv) >= min_vin_for_switching_uv(vbat_uv);
    if headroom_ok {
        Some(OpMode::Switching)
    } else if vin_uv >= CHARGE_MIN_VIN_UV && vin_uv < ELEVATED_MIN_VIN_UV {
        Some(OpMode::Bypass)
    } else {
        None
    }
}

/// Whether 1:1 (bypass) is allowed at this input - the only gate for `EN_1TO1`.
///
/// `EN_1TO1` connects the input straight to the battery, so the mode is allowed
/// only in the bypass window (`CHARGE_MIN_VIN_UV … ELEVATED_MIN_VIN_UV`). At a
/// raised Vin (QC/PD) it is forbidden: `Vin − Vbat` across the pass FETs now,
/// and the cell's own limit a step later.
///
/// The predicate is common to every path that can enable 1:1: mode selection in
/// [`charge_mode`], thermal protection, the `SET_MODE` IOCTL and recovery after
/// `soft_reset`.
#[must_use]
pub const fn bypass_allowed_by_vin(vin_uv: i32, vbat_uv: u32) -> bool {
    matches!(charge_mode(vin_uv, vbat_uv), Some(OpMode::Bypass))
}

/// Watchdog timer period duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogPeriod {
    /// 5 seconds.
    Sec5,
    /// 10 seconds.
    Sec10,
    /// 20 seconds.
    Sec20,
    /// 40 seconds.
    Sec40,
}

impl WatchdogPeriod {
    /// Period code for bits 5:6 of the `TIMER_CTRL` register.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Sec5 => 0x0,
            Self::Sec10 => 0x1,
            Self::Sec20 => 0x2,
            Self::Sec40 => 0x3,
        }
    }

    /// Period in seconds.
    #[must_use]
    pub const fn seconds(self) -> u8 {
        match self {
            Self::Sec5 => 5,
            Self::Sec10 => 10,
            Self::Sec20 => 20,
            Self::Sec40 => 40,
        }
    }
}

/// ADC hibernation entry delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcHibernateDelay {
    /// 500 ms.
    Ms500,
    /// 1 second.
    Sec1,
    /// 2 seconds.
    Sec2,
    /// 4 seconds.
    Sec4,
}

impl AdcHibernateDelay {
    /// Code for bits 3:4 of the `ADC_CTRL` register.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Ms500 => 0x0,
            Self::Sec1 => 0x1,
            Self::Sec2 => 0x2,
            Self::Sec4 => 0x3,
        }
    }
}

/// ADC operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcMode {
    /// Automatic hibernation.
    AutoHibernate,
    /// Automatic shutdown.
    AutoShutdown,
    /// Forced off.
    Shutdown,
    /// Forced hibernation.
    Hibernate,
    /// Normal measurement mode.
    Normal,
}

impl AdcMode {
    /// Code for bits 5:7 of the `ADC_CTRL` register.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::AutoHibernate => 0x0,
            Self::AutoShutdown => 0x1,
            Self::Shutdown => 0x2,
            Self::Hibernate => 0x4,
            Self::Normal => 0x6,
        }
    }
}

/// Upper bound of the input current encoding (7 bits).
pub const IIN_CODE_MAX: u8 = 0x7F;

/// Encodes the input current limit.
///
/// Code = `current / 50 mA` (see `ln8000_set_iin_limit`). As in the driver, the
/// value is clamped from above by the register field; from below the effective
/// minimum is [`regs::IIN_MIN_UA`].
///
/// # Errors
///
/// [`PumpError::OutOfRange`] if a zero or below-minimum current is requested.
pub fn encode_iin_limit(iin_ua: u32) -> Result<u8, PumpError> {
    if iin_ua < regs::IIN_MIN_UA {
        return Err(PumpError::OutOfRange {
            field: "iin_limit_ua",
            requested: iin_ua,
        });
    }
    let code = iin_ua / regs::IIN_STEP_UA;
    Ok(if code > u32::from(IIN_CODE_MAX) {
        IIN_CODE_MAX
    } else {
        u8::try_from(code).unwrap_or(IIN_CODE_MAX)
    })
}

/// Decodes the input current limit applied by the device.
#[must_use]
pub fn decode_iin_limit(raw: u8) -> u32 {
    let value = u32::from(raw & IIN_CODE_MAX).saturating_mul(regs::IIN_STEP_UA);
    if value < regs::IIN_MIN_UA {
        regs::IIN_MIN_UA
    } else {
        value
    }
}

/// Encodes the charge target voltage.
///
/// Code = `(voltage − 3.725 V) / 5 mV`, saturating at the range bounds.
#[must_use]
pub fn encode_vbat_float(vbat_uv: u32) -> u8 {
    if vbat_uv <= regs::VBAT_FLOAT_MIN_UV {
        return 0x00;
    }
    if vbat_uv >= regs::VBAT_FLOAT_MAX_UV {
        return 0xFF;
    }
    let steps = vbat_uv.saturating_sub(regs::VBAT_FLOAT_MIN_UV) / regs::VBAT_FLOAT_STEP_UV;
    u8::try_from(steps).unwrap_or(0xFF)
}

/// Decodes the charge voltage from the code.
#[must_use]
pub fn decode_vbat_float(raw: u8) -> u32 {
    regs::VBAT_FLOAT_MIN_UV.saturating_add(u32::from(raw).saturating_mul(regs::VBAT_FLOAT_STEP_UV))
}

/// Encodes the input overvoltage threshold (field 3:2 of the `GLITCH_CTRL` register).
#[must_use]
pub const fn encode_vac_ovp(ovp_uv: u32) -> u8 {
    if ovp_uv <= 6_500_000 {
        regs::VAC_OVP_6V5
    } else if ovp_uv <= 11_000_000 {
        regs::VAC_OVP_11V
    } else if ovp_uv <= 12_000_000 {
        regs::VAC_OVP_12V
    } else {
        regs::VAC_OVP_13V
    }
}

/// Encodes the NTC alarm threshold (10 bits: 8 in `NTC_CTRL`, 2 in `ADC_CTRL`).
#[must_use]
pub const fn encode_ntc_alarm(code: u16) -> (u8, u8) {
    ((code & 0xFF) as u8, ((code >> 8) & 0x03) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opmode_decoding_follows_status_bits() {
        assert_eq!(
            OpMode::from_sys_sts(regs::SYS_STS_SHUTDOWN),
            OpMode::Standby
        );
        assert_eq!(OpMode::from_sys_sts(regs::SYS_STS_STANDBY), OpMode::Standby);
        assert_eq!(
            OpMode::from_sys_sts(regs::SYS_STS_SWITCHING_ENABLED),
            OpMode::Switching
        );
        assert_eq!(
            OpMode::from_sys_sts(regs::SYS_STS_BYPASS_ENABLED),
            OpMode::Bypass
        );
        assert_eq!(OpMode::from_sys_sts(0), OpMode::Unknown);
        // The regulation loop bit must not confuse mode parsing.
        assert_eq!(
            OpMode::from_sys_sts(regs::SYS_STS_SWITCHING_ENABLED | regs::SYS_STS_IIN_LOOP),
            OpMode::Switching
        );
    }

    #[test]
    fn opmode_encoding_matches_driver() {
        assert_eq!(OpMode::Standby.sys_ctrl_bits().unwrap(), 1 << 3);
        assert_eq!(OpMode::Bypass.sys_ctrl_bits().unwrap(), 1 << 0);
        assert_eq!(OpMode::Switching.sys_ctrl_bits().unwrap(), 0);
        assert!(OpMode::Unknown.sys_ctrl_bits().is_err());
        assert_eq!(OpMode::sys_ctrl_mask(), (1 << 3) | (1 << 0));
    }

    #[test]
    fn iin_limit_round_trip() {
        assert_eq!(encode_iin_limit(500_000).unwrap(), 10);
        assert_eq!(encode_iin_limit(2_000_000).unwrap(), 40);
        assert_eq!(encode_iin_limit(3_000_000).unwrap(), 60);
        assert_eq!(encode_iin_limit(9_000_000).unwrap(), IIN_CODE_MAX);
        assert_eq!(decode_iin_limit(60), 3_000_000);
        assert_eq!(decode_iin_limit(0), regs::IIN_MIN_UA);
    }

    #[test]
    fn iin_limit_rejects_too_low() {
        let err = encode_iin_limit(100_000).unwrap_err();
        assert!(matches!(err, PumpError::OutOfRange { .. }));
    }

    #[test]
    fn vbat_float_clamps_at_bounds() {
        assert_eq!(encode_vbat_float(3_000_000), 0x00);
        assert_eq!(encode_vbat_float(regs::VBAT_FLOAT_MIN_UV), 0x00);
        assert_eq!(encode_vbat_float(4_440_000), 143);
        assert_eq!(encode_vbat_float(NABU_VBAT_FLOAT_UV), 149);
        assert_eq!(encode_vbat_float(9_000_000), 0xFF);
        assert_eq!(decode_vbat_float(143), 4_440_000);
        assert_eq!(decode_vbat_float(149), NABU_VBAT_FLOAT_UV);
    }

    #[test]
    fn nabu_targets_encode_to_expected_codes() {
        // dtbo-03: non-FFC target 4450 mV → 0x91; FFC fv-max 4470 mV → 0x95.
        assert_eq!(NABU_VBAT_NON_FFC_UV, 4_450_000);
        assert_eq!(encode_vbat_float(NABU_VBAT_NON_FFC_UV), 0x91);
        assert_eq!(encode_vbat_float(NABU_VBAT_FLOAT_UV), 0x95);
        assert_eq!(decode_vbat_float(0x91), NABU_VBAT_NON_FFC_UV);
        assert_eq!(decode_vbat_float(0x95), NABU_VBAT_FLOAT_UV);
        // The QC3 loop limit (4.42 V) is not a charge target.
        assert_eq!(encode_vbat_float(NABU_QC3_BAT_VOLT_MAX_UV), 0x8B);
    }

    #[test]
    fn soft_float_raises_near_full_vbat() {
        assert_eq!(
            soft_float_for_vbat(NABU_VBAT_FLOAT_UV, 4_000_000),
            NABU_VBAT_FLOAT_UV
        );
        assert_eq!(
            soft_float_for_vbat(NABU_VBAT_FLOAT_UV, 4_520_000),
            VBAT_FLOAT_SOFT_MAX_UV
        );
        assert!(vbat_near_float(4_520_000, NABU_VBAT_FLOAT_UV, false));
        assert!(vbat_near_float(4_000_000, NABU_VBAT_FLOAT_UV, true));
        assert!(!vbat_near_float(4_000_000, NABU_VBAT_FLOAT_UV, false));
        // Live ws-e4: switching VBAT≈4780 mV with Vin≈9616 mV is Vin/2 rail, not float.
        assert!(vbat_tracks_converter_rail(4_780_000, 9_616_000));
        assert!(!vbat_near_float_with_vin(
            4_780_000,
            NABU_VBAT_FLOAT_UV,
            false,
            9_616_000
        ));
        // Standby pack reading at true float is still near-float.
        assert!(vbat_near_float_with_vin(
            4_470_000,
            NABU_VBAT_FLOAT_UV,
            false,
            9_616_000
        ));
    }

    #[test]
    fn vac_ovp_thresholds() {
        assert_eq!(encode_vac_ovp(6_500_000), regs::VAC_OVP_6V5);
        assert_eq!(encode_vac_ovp(9_500_000), regs::VAC_OVP_11V);
        assert_eq!(encode_vac_ovp(11_500_000), regs::VAC_OVP_12V);
        assert_eq!(encode_vac_ovp(13_000_000), regs::VAC_OVP_13V);
    }

    #[test]
    fn charge_mode_follows_vin_and_battery_headroom() {
        // Vin too low for anything.
        assert_eq!(charge_mode(4_000_000, 4_000_000), None);
        assert_eq!(charge_mode(4_199_999, 4_000_000), None);
        // 5 V path: bypass while Vin stays below the elevated floor.
        assert_eq!(charge_mode(4_200_000, 4_000_000), Some(OpMode::Bypass));
        assert_eq!(charge_mode(4_448_000, 4_000_000), Some(OpMode::Bypass)); // TA200 under load
        assert_eq!(charge_mode(5_000_000, 4_000_000), Some(OpMode::Bypass));
        assert_eq!(charge_mode(5_999_999, 4_000_000), Some(OpMode::Bypass));
        // A 4.0 V pack gates 2:1 at 8.25 V; below the elevated floor drains to
        // standby, not to 1:1 (4.0 V on the cell from an 8 V bus).
        assert_eq!(charge_mode(6_000_000, 4_000_000), None);
        assert_eq!(charge_mode(7_999_999, 4_000_000), None);
        assert_eq!(charge_mode(8_000_000, 4_000_000), None);
        assert_eq!(charge_mode(8_249_999, 4_000_000), None);
        assert_eq!(charge_mode(8_250_000, 4_000_000), Some(OpMode::Switching));
        assert_eq!(charge_mode(9_000_000, 4_200_000), Some(OpMode::Switching));
        assert_eq!(charge_mode(12_336_000, 4_000_000), Some(OpMode::Switching));
    }

    #[test]
    fn low_pack_admits_2to1_inside_its_own_band() {
        // Live 24.09: cell 3,68–3,70 V (`FgVbattMv` 3704 against `VbatAdcMv`
        // 3695, standby), band [7,56; 7,80] V — every point of it below the old
        // 8,0 V admission floor, which is why the driver could not hold the
        // transfer there.
        let vbat = 3_700_000;
        assert_eq!(min_vin_for_switching_uv(vbat), 7_650_000);
        // The band centre (the target the correction aims at) is admitted.
        assert_eq!(window_target_uv(vbat), 7_700_000);
        assert_eq!(charge_mode(7_700_000, vbat), Some(OpMode::Switching));
        // Admission keeps the vendor 250 mV gate while the band floor is 200 mV
        // above 2*Vbat, so the lowest 50 mV of the band is refused — the same
        // asymmetry as before, now sitting 350 mV lower for this pack.
        assert_eq!(charge_mode(7_600_000, vbat), None);
        // Above the band the bus is still admitted (2:1 is not refused for being
        // too high — the pump carries the floor there and the correction owns
        // the bus), and the elevated floor no longer 1:1's it.
        assert_eq!(charge_mode(7_800_000, vbat), Some(OpMode::Switching));
        assert_eq!(charge_mode(8_000_000, vbat), Some(OpMode::Switching));
    }

    #[test]
    fn elevated_vin_without_headroom_is_never_bypass() {
        // Live nabu: PD brick 8.416 V while the pack sits at 4.275 V.
        // 2:1 needs >= 8.8 V, and 1:1 would put 8.4 V across the battery.
        assert_eq!(min_vin_for_switching_uv(4_275_000), 8_800_000);
        assert_eq!(charge_mode(8_416_000, 4_275_000), None);
        // A bus in the 6–8 V hole is standby, never 1:1: live 24.09 the 1:1
        // selected there carried 2,7 A with 2,9 V across the pass FETs and
        // sagged the bus to 6,7 V.
        assert_eq!(charge_mode(6_700_000, 3_700_000), None);
        assert_eq!(charge_mode(7_500_000, 3_500_000), Some(OpMode::Switching));
        assert_eq!(charge_mode(7_249_999, 3_500_000), None);
        assert_eq!(charge_mode(7_250_000, 3_500_000), Some(OpMode::Switching));
        // Sanity: no 1:1 selection at or above the elevated floor.
        for vin in (6_000_000..12_000_000).step_by(250_000) {
            assert_ne!(
                charge_mode(vin, 4_200_000),
                Some(OpMode::Bypass),
                "bypass must never be picked at Vin {vin}"
            );
        }
    }

    #[test]
    fn hvdcp_bus_target_admits_switching_across_the_pack_range() {
        // Host-side mirror of `hvdcp::target_vbus_uv`: the mid-band target,
        // clamped up to the elevated floor (`ELEVATED_MIN_VIN_UV`, 6 V) so a
        // very low pack still admits the mode. The KMDF crate is `no_std` with
        // `panic=abort`, so its `#[cfg(test)]` tests cannot execute — this one
        // stands in for them.
        for vbat in (3_000_000..=4_500_000).step_by(50_000) {
            let target = window_target_uv(vbat).max(ELEVATED_MIN_VIN_UV as u32);
            // The window is a few volts, so the conversion never saturates.
            let vin_uv = i32::try_from(target).unwrap_or(i32::MAX);
            assert_eq!(
                charge_mode(vin_uv, vbat),
                Some(OpMode::Switching),
                "target {target} must allow 2:1 at Vbat {vbat}"
            );
            // Never above the band top unless the elevated floor pins it there.
            assert!(
                target <= window_top_uv(vbat).max(ELEVATED_MIN_VIN_UV as u32),
                "target {target} is above the window at Vbat {vbat}"
            );
        }
    }

    #[test]
    fn switching_window_matches_the_live_band() {
        // Live 18.09, pack 4.42–4.44 V: 9088 mV → 1887 mA and 9280 mV →
        // 2513 mA carry power; 9744 mV and 9888 mV (mode 3, `SYS_STS=0x04`)
        // carry only the 39 mA ADC floor.
        assert!(vin_in_switching_window(9_088_000, 4_420_000));
        assert!(vin_in_switching_window(9_280_000, 4_440_000));
        assert!(!vin_in_switching_window(9_744_000, 4_420_000));
        assert!(!vin_in_switching_window(9_888_000, 4_420_000));
        // The derived target is inside the band; the old fixed 9.5 V floor —
        // three QC3 steps above the top at this pack voltage — is not. That gap
        // is why the driver's own telemetry killed the 2:1 state it had just
        // been handed.
        assert!(vin_in_switching_window(
            i32::try_from(window_target_uv(4_420_000)).unwrap_or(i32::MAX),
            4_420_000
        ));
        assert!(!vin_in_switching_window(9_500_000, 4_420_000));
        // An unknown pack or absent input is never "in band".
        assert!(!vin_in_switching_window(9_500_000, 0));
        assert!(!vin_in_switching_window(0, 4_420_000));
    }

    #[test]
    fn bypass_window_covers_only_the_five_volt_side() {
        // Single gate for every path that enables EN_1TO1 (thermal protection,
        // SET_MODE, recovery after soft_reset): the 4.2-6.0 V window, regardless of
        // Vbat. The upper bound is the elevated boundary — above it the input is a
        // QC/PD bus, and 1:1 would put `Vin − Vbat` across the pass FETs (live
        // 24.09: 2,7 A with 2,9 V there, the bus sagging to 6,7 V).
        for vin in [4_200_000, 4_500_000, 5_000_000, 5_999_999] {
            for vbat in [0, 3_900_000, 4_470_000] {
                assert!(
                    bypass_allowed_by_vin(vin, vbat),
                    "Vin {vin} at Vbat {vbat} - bypass window"
                );
            }
        }
        for vin in [
            6_000_000, 6_700_000, 7_999_999, 8_416_000, 9_000_000, 12_000_000,
        ] {
            for vbat in [0, 3_900_000, 4_275_000, 4_470_000] {
                assert!(
                    !bypass_allowed_by_vin(vin, vbat),
                    "Vin {vin} at Vbat {vbat} - 1:1 feeds the input to the battery"
                );
            }
        }
        // Below the bypass window is also not allowed: 1:1 from a near-zero input is useless.
        assert!(!bypass_allowed_by_vin(4_199_999, 4_000_000));
        assert!(!bypass_allowed_by_vin(0, 4_000_000));
        assert!(!bypass_allowed_by_vin(-1, 4_000_000));
    }

    #[test]
    fn taper_band_follows_charge_target_not_qc3_limit() {
        // F5: the fold-back band is set by the charge target (float - 100 mV), not by the
        // QC3 loop limit (4.42 V). Otherwise a 4.47 V target already cuts current from 4.32 V.
        assert_eq!(NABU_QC3_BAT_VOLT_MAX_UV, 4_420_000);
        // Target 4.47 V → fold-back from 4.37 V.
        assert!(!vbat_near_float_with_vin(
            4_350_000,
            NABU_VBAT_FLOAT_UV,
            false,
            0
        ));
        assert!(!vbat_near_float_with_vin(
            4_369_999,
            NABU_VBAT_FLOAT_UV,
            false,
            0
        ));
        assert!(vbat_near_float_with_vin(
            4_370_000,
            NABU_VBAT_FLOAT_UV,
            false,
            0
        ));
        assert!(vbat_near_float_with_vin(
            4_460_000,
            NABU_VBAT_FLOAT_UV,
            false,
            0
        ));
        // Non-FFC target 4.45 V → fold-back from 4.35 V, still not 4.32 V.
        assert!(!vbat_near_float_with_vin(
            4_340_000,
            NABU_VBAT_NON_FFC_UV,
            false,
            0
        ));
        assert!(vbat_near_float_with_vin(
            4_350_000,
            NABU_VBAT_NON_FFC_UV,
            false,
            0
        ));
        // If the target is the QC3 loop limit itself, the band follows it.
        assert!(!vbat_near_float_with_vin(
            4_319_999,
            NABU_QC3_BAT_VOLT_MAX_UV,
            false,
            0
        ));
        assert!(vbat_near_float_with_vin(
            4_320_000,
            NABU_QC3_BAT_VOLT_MAX_UV,
            false,
            0
        ));
        // Latched VBAT_OV and the Vin/2 rail are handled as before.
        assert!(vbat_near_float_with_vin(
            4_000_000,
            NABU_VBAT_FLOAT_UV,
            true,
            0
        ));
        assert!(!vbat_near_float_with_vin(
            4_808_000,
            NABU_VBAT_FLOAT_UV,
            false,
            9_616_000
        ));
    }

    #[test]
    fn ntc_alarm_splits_into_two_registers() {
        let (low, high) = encode_ntc_alarm(226);
        assert_eq!(low, 226);
        assert_eq!(high, 0);
        let (low, high) = encode_ntc_alarm(0x2FF);
        assert_eq!(low, 0xFF);
        assert_eq!(high, 0x02);
    }

    #[test]
    fn watchdog_and_adc_codes() {
        assert_eq!(WatchdogPeriod::Sec5.code(), 0);
        assert_eq!(WatchdogPeriod::Sec40.code(), 3);
        assert_eq!(WatchdogPeriod::Sec10.seconds(), 10);
        assert_eq!(AdcHibernateDelay::Sec4.code(), 3);
        assert_eq!(AdcMode::Shutdown.code(), 2);
        assert_eq!(AdcMode::AutoHibernate.code(), 0);
    }

    #[test]
    fn hibernated_adc_zero_is_not_a_usable_vbat_reading() {
        // Live measurement 19.09, cable unplugged: `ADC_CTRL = 0x1C`. Bits 5:7 = 0 =
        // `AutoHibernate`, bits 3:4 = 3 = `Sec4` - that is exactly what
        // initialisation step 9 writes. After four seconds of idle `ADC01..ADC09`
        // read successfully and contain `0x00`.
        assert_eq!(0x1Cu8 >> 5, AdcMode::AutoHibernate.code());
        assert_eq!((0x1Cu8 >> 3) & 0x03, AdcHibernateDelay::Sec4.code());
        // A successfully read zero is still not a cell sample.
        assert!(!vbat_reading_usable(true, 0));
        assert!(vbat_reading_usable(true, 4_400_000));
        // A failed read is invalid for any value.
        assert!(!vbat_reading_usable(false, 4_400_000));
        assert!(!vbat_reading_usable(false, 0));
    }

    #[test]
    fn a_live_transfer_is_walked_up_but_never_down() {
        // A dead transfer is the one case that always asks for a correction, in
        // both directions of the band.
        assert!(should_walk_window(true, false, false));
        assert!(should_walk_window(true, true, false));
        assert!(should_walk_window(true, true, true));
        // Nothing flowing: the bus can be placed.
        assert!(should_walk_window(false, false, false));
        // Carrying and above the floor: leave it alone. Live 24.09 the driver
        // pulsed exactly here (`WindowDead = 0`, `BoostInc` 6, 2,2 A in the same
        // window) and walked the bus 7,6 -> 8,9 V out of the band.
        assert!(!should_walk_window(false, true, false));
        // Carrying and dragged below the floor by its own load: raise it - the
        // sag is the load's signature, and one INC step is what recovers the
        // current (live 676: 0,8 A at 7,90 V loaded against a 7,97 V floor).
        assert!(should_walk_window(false, true, true));
        // The carrier test sits above the LN8000 `Iin` floor (39 mA, the pump's
        // zero) and below every measured working point.
        assert_eq!(IIN_USEFUL_UA, 300_000);
        assert!(!transfer_is_useful(39_000));
        assert!(!transfer_is_useful(IIN_USEFUL_UA));
        assert!(transfer_is_useful(680_000));
        assert!(transfer_is_useful(2_200_000));
        assert!(transfer_is_useful(2_760_000));
    }

    #[test]
    fn a_live_transfer_outranks_the_sampled_band_verdict() {
        // An admissible bus: nothing to stop.
        assert!(!stop_running_charge(true, false));
        assert!(!stop_running_charge(true, true));
        // Not admissible and nothing flowing: the charge is stopped, as before.
        assert!(stop_running_charge(false, false));
        // Not admissible but carrying: left running. Live 24.09 this sample
        // (`SuMode = 3`, 0,9 A, loaded bus 7,80 V against an 8,00 V gate) is what
        // switched the charge off and started the flap.
        assert!(!stop_running_charge(false, true));
    }
}
