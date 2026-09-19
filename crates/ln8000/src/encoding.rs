//! Кодирование и декодирование значений LN8000.
//!
//! Повторяет функции драйвера `ln8000_charger.c`, чтобы под Windows значения
//! кодировались так же, как под Android.

use crate::error::PumpError;
use crate::regs;

/// Рабочий режим charge pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpMode {
    /// Состояние не определено.
    Unknown,
    /// Ожидание: ключи выключены, ток не идёт.
    Standby,
    /// Режим 1:1 — вход идёт на батарею напрямую (5 В).
    Bypass,
    /// Режим 2:1 — понижающий преобразователь (9 В → 4.5 В).
    Switching,
}

impl OpMode {
    /// Код режима, как в `enum ln8000_opmode_` из драйвера.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0x0,
            Self::Standby => 0x1,
            Self::Bypass => 0x2,
            Self::Switching => 0x3,
        }
    }

    /// Имя для журнала.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Standby => "STANDBY",
            Self::Bypass => "BYPASS",
            Self::Switching => "SWITCHING",
        }
    }

    /// Разбирает режим по значению регистра `SYS_STS`.
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

    /// Значение порции `SYS_CTRL` для переключения в этот режим.
    ///
    /// Маска всегда одна: `STANDBY_EN | EN_1TO1` (см. `ln8000_change_opmode`).
    ///
    /// # Errors
    ///
    /// [`PumpError::OutOfRange`], если режим [`OpMode::Unknown`] переключить нельзя.
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

    /// Маска битов `SYS_CTRL`, которыми управляет режим.
    #[must_use]
    pub const fn sys_ctrl_mask() -> u8 {
        regs::SYS_CTRL_STANDBY_EN | regs::SYS_CTRL_EN_1TO1
    }
}

/// Absolute minimum pump input for 2:1 switching (µV).
///
/// The live gate is [`min_vin_for_switching_uv`] (`2 * Vbat + 250 mV`, Android
/// `cp_qc30.c` `VBUS_COMP` for the LN8000 build); this constant stays as the
/// absolute floor below which 2:1 is never requested. Below it the 1:1 bypass
/// is used for a ~5 V input.
pub const SWITCHING_MIN_VIN_UV: i32 = 8_000_000;

/// Headroom above `2 * Vbat` required to enter 2:1 (µV).
///
/// Android `cp_qc30.c` `VBUS_COMP` = 250 mV in the `CONFIG_CHARGER_LN8000`
/// build: the charge pump cracks only when `Vbus - 2*Vbat` has real margin.
pub const SWITCHING_HEADROOM_UV: u32 = 250_000;

/// Minimum Vin that can physically feed 2:1 at `vbat_uv` (µV).
///
/// Live nabu case: a PD brick at 8.416 V with the pack at 4.275 V cannot run
/// 2:1 (needs ≥ 2·4.275 + 0.25 = 8.8 V) — `enable_switching` then returns
/// `ModeNotReached` every tick. Exit to standby instead of retrying.
#[must_use]
pub const fn min_vin_for_switching_uv(vbat_uv: u32) -> u32 {
    vbat_uv
        .saturating_mul(2)
        .saturating_add(SWITCHING_HEADROOM_UV)
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
    vin_uv >= window_floor_uv(vbat_uv) as i32 && vin_uv <= window_top_uv(vbat_uv) as i32
}

/// Non-negative Vin in µV for unsigned comparisons (`0` for absent / negative).
#[must_use]
pub const fn non_negative_uv(vin_uv: i32) -> u32 {
    if vin_uv > 0 { vin_uv.unsigned_abs() } else { 0 }
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

/// Насколько должен измениться Vin, чтобы POR-бюджет считался новым входом.
///
/// Запас в 200 мВ выбран по живому разбросу: один и тот же блок на 5 В даёт
/// 4,98–5,05 В (в пределах запаса, POR не повторяется), а переход 5 В → 9 В
/// (QC3/PD) меняет вход на вольты и открывает новый бюджет.
pub const POR_VIN_TOLERANCE_UV: u32 = 200_000;

/// True when `Vin` is the converter's own `2 · VBAT` reflection, not an adapter.
///
/// Отличие от [`vbat_tracks_converter_rail`] — нет порога 8 В: отражение
/// масштабируется вместе с банкой, и на разряженной банке (2,1–4,0 В) попадает
/// в 4,2–8,0 В, то есть ровно в ту полосу, где `vbat_tracks_converter_rail`
/// молчит, а [`crate::battery_policy::online_raw`] решает судьбу `POWER_ON_LINE`.
///
/// Живой случай 19.09: кабель отключён, `Vin = 8 800 000`, `VBAT = 4 400 000`
/// (ровно вдвое), ток на полу АЦП 39 мА, режим Standby — и трей показывал
/// «подключён», пока пак разряжался.
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
/// * `Vin >= 2*Vbat + 250 mV` (and `Vin >= SWITCHING_MIN_VIN_UV`) → 2:1
/// * `CHARGE_MIN_VIN_UV .. SWITCHING_MIN_VIN_UV` → 1:1 bypass (5 V path)
/// * elevated but no headroom, or Vin too low → `None` (standby)
///
/// The bypass is **never** selected at `Vin >= SWITCHING_MIN_VIN_UV`: 1:1 feeds
/// the input straight to the pack, so 8 V+ there would be a battery overvoltage.
#[must_use]
pub const fn charge_mode(vin_uv: i32, vbat_uv: u32) -> Option<OpMode> {
    let headroom_ok = non_negative_uv(vin_uv) >= min_vin_for_switching_uv(vbat_uv);
    if vin_uv >= SWITCHING_MIN_VIN_UV && headroom_ok {
        Some(OpMode::Switching)
    } else if vin_uv >= CHARGE_MIN_VIN_UV && vin_uv < SWITCHING_MIN_VIN_UV {
        Some(OpMode::Bypass)
    } else {
        None
    }
}

/// Разрешён ли 1:1 (bypass) при таком входе — единственный гейт для `EN_1TO1`.
///
/// `EN_1TO1` соединяет вход с батареей напрямую, поэтому режим допустим только в
/// окне обхода (`CHARGE_MIN_VIN_UV … SWITCHING_MIN_VIN_UV`). На повышенном Vin
/// (QC/PD) он запрещён: 8 В и выше на банке — это перенапряжение.
///
/// Предикат общий для всех путей, которые могут включить 1:1: выбор режима в
/// [`charge_mode`], термозащита, IOCTL `SET_MODE` и восстановление после
/// `soft_reset`.
#[must_use]
pub const fn bypass_allowed_by_vin(vin_uv: i32, vbat_uv: u32) -> bool {
    matches!(charge_mode(vin_uv, vbat_uv), Some(OpMode::Bypass))
}

/// Длительность периода сторожевого таймера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogPeriod {
    /// 5 секунд.
    Sec5,
    /// 10 секунд.
    Sec10,
    /// 20 секунд.
    Sec20,
    /// 40 секунд.
    Sec40,
}

impl WatchdogPeriod {
    /// Код периода для битов 5:6 регистра `TIMER_CTRL`.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Sec5 => 0x0,
            Self::Sec10 => 0x1,
            Self::Sec20 => 0x2,
            Self::Sec40 => 0x3,
        }
    }

    /// Период в секундах.
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

/// Задержка перехода АЦП в гибернацию.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcHibernateDelay {
    /// 500 мс.
    Ms500,
    /// 1 секунда.
    Sec1,
    /// 2 секунды.
    Sec2,
    /// 4 секунды.
    Sec4,
}

impl AdcHibernateDelay {
    /// Код для битов 3:4 регистра `ADC_CTRL`.
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

/// Режим работы АЦП.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcMode {
    /// Автоматически в гибернацию.
    AutoHibernate,
    /// Автоматически в shutdown.
    AutoShutdown,
    /// Принудительно выключен.
    Shutdown,
    /// Принудительно в гибернации.
    Hibernate,
    /// Обычный режим измерений.
    Normal,
}

impl AdcMode {
    /// Код для битов 5:7 регистра `ADC_CTRL`.
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

/// Верхняя граница кодирования входного тока (7 бит).
pub const IIN_CODE_MAX: u8 = 0x7F;

/// Кодирует лимит входного тока.
///
/// Код = `ток / 50 мА` (см. `ln8000_set_iin_limit`). Как и в драйвере,
/// значение ограничивается сверху полем регистра; снизу эффективный минимум —
/// [`regs::IIN_MIN_UA`].
///
/// # Errors
///
/// [`PumpError::OutOfRange`], если запрошен нулевой или меньший минимального ток.
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

/// Декодирует лимит входного тока, применённый устройством.
#[must_use]
pub fn decode_iin_limit(raw: u8) -> u32 {
    let value = u32::from(raw & IIN_CODE_MAX).saturating_mul(regs::IIN_STEP_UA);
    if value < regs::IIN_MIN_UA {
        regs::IIN_MIN_UA
    } else {
        value
    }
}

/// Кодирует целевое напряжение заряда.
///
/// Код = `(напряжение − 3.725 В) / 5 мВ`, с насыщением на границах диапазона.
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

/// Декодирует напряжение заряда из кода.
#[must_use]
pub fn decode_vbat_float(raw: u8) -> u32 {
    regs::VBAT_FLOAT_MIN_UV.saturating_add(u32::from(raw).saturating_mul(regs::VBAT_FLOAT_STEP_UV))
}

/// Кодирует порог перенапряжения входа (поле 3:2 регистра `GLITCH_CTRL`).
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

/// Кодирует порог аларма NTC (10 бит: 8 в `NTC_CTRL`, 2 в `ADC_CTRL`).
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
        // Бит петли регулирования не должен путать разбор режима.
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
        // 5 V path: bypass while Vin stays below the 2:1 floor.
        assert_eq!(charge_mode(4_200_000, 4_000_000), Some(OpMode::Bypass));
        assert_eq!(charge_mode(4_448_000, 4_000_000), Some(OpMode::Bypass)); // TA200 under load
        assert_eq!(charge_mode(5_000_000, 4_000_000), Some(OpMode::Bypass));
        assert_eq!(charge_mode(7_999_999, 4_000_000), Some(OpMode::Bypass));
        // 2:1 needs 2*Vbat + 250 mV: exactly 8.25 V at Vbat 4.0 V.
        assert_eq!(charge_mode(8_000_000, 4_000_000), None);
        assert_eq!(charge_mode(8_249_999, 4_000_000), None);
        assert_eq!(charge_mode(8_250_000, 4_000_000), Some(OpMode::Switching));
        assert_eq!(charge_mode(9_000_000, 4_200_000), Some(OpMode::Switching));
        assert_eq!(charge_mode(12_336_000, 4_000_000), Some(OpMode::Switching));
    }

    #[test]
    fn elevated_vin_without_headroom_is_never_bypass() {
        // Live nabu: PD brick 8.416 V while the pack sits at 4.275 V.
        // 2:1 needs >= 8.8 V, and 1:1 would put 8.4 V across the battery.
        assert_eq!(min_vin_for_switching_uv(4_275_000), 8_800_000);
        assert_eq!(charge_mode(8_416_000, 4_275_000), None);
        // Absolute floor still binds for a low pack: 7.5 V >= 2*3.5+0.25 V.
        assert_eq!(charge_mode(7_500_000, 3_500_000), Some(OpMode::Bypass));
        assert_eq!(charge_mode(8_000_000, 3_500_000), Some(OpMode::Switching));
        // Sanity: no 1:1 selection at or above the 2:1 floor.
        for vin in (8_000_000..12_000_000).step_by(250_000) {
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
        // clamped up to the absolute 2:1 floor so a low pack still admits the
        // mode. The KMDF crate is `no_std` with `panic=abort`, so its
        // `#[cfg(test)]` tests cannot execute — this one stands in for them.
        for vbat in (3_000_000..=4_500_000).step_by(50_000) {
            let target = window_target_uv(vbat).max(SWITCHING_MIN_VIN_UV as u32);
            assert_eq!(
                charge_mode(target as i32, vbat),
                Some(OpMode::Switching),
                "цель {target} обязана допускать 2:1 при Vbat {vbat}"
            );
            // Never above the band top unless the absolute floor pins it there.
            assert!(
                target <= window_top_uv(vbat).max(SWITCHING_MIN_VIN_UV as u32),
                "цель {target} выше окна при Vbat {vbat}"
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
            window_target_uv(4_420_000) as i32,
            4_420_000
        ));
        assert!(!vin_in_switching_window(9_500_000, 4_420_000));
        // An unknown pack or absent input is never "in band".
        assert!(!vin_in_switching_window(9_500_000, 0));
        assert!(!vin_in_switching_window(0, 4_420_000));
    }

    #[test]
    fn bypass_window_covers_only_the_five_volt_side() {
        // Единый гейт для всех путей, включающих EN_1TO1 (термозащита, SET_MODE,
        // восстановление после soft_reset): окно 4,2…8 В, независимо от Vbat.
        for vin in [4_200_000, 4_500_000, 5_000_000, 7_999_999] {
            for vbat in [0, 3_900_000, 4_470_000] {
                assert!(
                    bypass_allowed_by_vin(vin, vbat),
                    "Vin {vin} при Vbat {vbat} — окно обхода"
                );
            }
        }
        for vin in [8_000_000, 8_416_000, 9_000_000, 12_000_000] {
            for vbat in [0, 3_900_000, 4_275_000, 4_470_000] {
                assert!(
                    !bypass_allowed_by_vin(vin, vbat),
                    "Vin {vin} при Vbat {vbat} — 1:1 подаёт вход на батарею"
                );
            }
        }
        // Ниже окна обхода тоже нельзя: 1:1 от почти нулевого входа бесполезен.
        assert!(!bypass_allowed_by_vin(4_199_999, 4_000_000));
        assert!(!bypass_allowed_by_vin(0, 4_000_000));
        assert!(!bypass_allowed_by_vin(-1, 4_000_000));
    }

    #[test]
    fn taper_band_follows_charge_target_not_qc3_limit() {
        // F5: полосу среза задаёт цель заряда (float − 100 мВ), а не лимит петли
        // QC3 (4,42 В). Иначе при цели 4,47 В ток режется уже с 4,32 В.
        assert_eq!(NABU_QC3_BAT_VOLT_MAX_UV, 4_420_000);
        // Цель 4,47 В → срез с 4,37 В.
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
        // Не-FFC цель 4,45 В → срез с 4,35 В, всё ещё не 4,32 В.
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
        // Если целью выбран сам лимит петли QC3, полоса едет за ним.
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
        // Защёлкнутый VBAT_OV и рэйл Vin/2 обрабатываются как раньше.
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
}
