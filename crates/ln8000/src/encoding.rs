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
        assert_eq!(encode_vbat_float(9_000_000), 0xFF);
        assert_eq!(decode_vbat_float(143), 4_440_000);
    }

    #[test]
    fn vac_ovp_thresholds() {
        assert_eq!(encode_vac_ovp(6_500_000), regs::VAC_OVP_6V5);
        assert_eq!(encode_vac_ovp(9_500_000), regs::VAC_OVP_11V);
        assert_eq!(encode_vac_ovp(11_500_000), regs::VAC_OVP_12V);
        assert_eq!(encode_vac_ovp(13_000_000), regs::VAC_OVP_13V);
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
