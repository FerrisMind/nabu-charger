//! Защита по температуре и току: решение, что делать с режимом зарядки.
//!
//! Драйвер обязан не только включать ускоренный режим, но и вовремя его
//! ограничивать. Модуль принимает последний отсчёт телеметрии и лимиты, а
//! возвращает действие: ничего не делать, снизить ток, уйти в bypass или
//! полностью прекратить заряд.
//!
//! Пороги задаются в одних единицах с телеметрией: температура — десятые доли
//! °C, ток — микроамперы, напряжение — микровольты.

use crate::session::TelemetrySample;

/// Шаг снижения тока при перегреве, мкА.
pub const CURRENT_STEP_UA: u32 = 250_000;

/// Лимиты защиты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardLimits {
    /// Температура, начиная с которой снижаем ток (десятые °C).
    pub temp_reduce_dc: i32,
    /// Температура, при которой уходим в bypass (десятые °C).
    pub temp_bypass_dc: i32,
    /// Температура, при которой заряд прекращается (десятые °C).
    pub temp_stop_dc: i32,
    /// Максимальный входной ток, мкА.
    pub iin_max_ua: u32,
    /// Целевой лимит входного тока, мкА.
    pub iin_target_ua: u32,
    /// Минимальный входной ток в bypass, мкА.
    pub iin_floor_ua: u32,
    /// Напряжение батареи, при котором снижаем ток (мкВ).
    pub vbat_reduce_uv: u32,
}

impl Default for GuardLimits {
    fn default() -> Self {
        Self::standard()
    }
}

impl GuardLimits {
    /// Профиль по умолчанию, доступный в константном контексте.
    ///
    /// Пороги взяты из практики мобильных платформ и значений драйвера
    /// Android: аларм NTC соответствует ≈ +40 °C, защита кристалла начинается
    /// задолго до аппаратного максимума (+160 °C).
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            temp_reduce_dc: 430,
            temp_bypass_dc: 480,
            temp_stop_dc: 550,
            iin_max_ua: 3_500_000,
            iin_target_ua: 2_000_000,
            iin_floor_ua: 500_000,
            vbat_reduce_uv: 4_420_000,
        }
    }

    /// Строгий профиль: зарядка без ускоренного режима.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            temp_reduce_dc: 400,
            temp_bypass_dc: 430,
            temp_stop_dc: 500,
            iin_max_ua: 1_500_000,
            iin_target_ua: 1_000_000,
            iin_floor_ua: 500_000,
            vbat_reduce_uv: 4_350_000,
        }
    }
}

/// Что делать с режимом зарядки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuardAction {
    /// Ничего не менять.
    None,
    /// Снизить лимит входного тока до указанного значения.
    ReduceCurrent {
        /// Новый лимит, мкА.
        to_ua: u32,
        /// Причина для журнала.
        reason: &'static str,
    },
    /// Уйти в режим bypass 1:1 (медленная, но безопасная зарядка).
    FallbackToBypass {
        /// Причина для журнала.
        reason: &'static str,
    },
    /// Полностью прекратить заряд (standby).
    Stop {
        /// Причина для журнала.
        reason: &'static str,
    },
}

impl GuardAction {
    /// Признак того, что действие требует записи в устройство.
    #[must_use]
    pub const fn is_change(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Короткое имя действия для журнала.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReduceCurrent { .. } => "reduce_current",
            Self::FallbackToBypass { .. } => "fallback_bypass",
            Self::Stop { .. } => "stop",
        }
    }
}

/// Считает действие защиты по последнему отсчёту.
#[must_use]
pub fn evaluate(sample: &TelemetrySample, limits: &GuardLimits) -> GuardAction {
    if !sample.input_present {
        return GuardAction::None;
    }

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
    if sample.die_temp_dc >= limits.temp_reduce_dc {
        return GuardAction::ReduceCurrent {
            to_ua: step_down(sample.iin_ua, limits),
            reason: "die_temp_reduce",
        };
    }

    if sample.iin_ua > limits.iin_max_ua {
        return GuardAction::ReduceCurrent {
            to_ua: limits.iin_target_ua,
            reason: "iin_over_limit",
        };
    }

    if sample.vbat_uv >= limits.vbat_reduce_uv {
        return GuardAction::ReduceCurrent {
            to_ua: step_down(sample.iin_ua, limits),
            reason: "vbat_reduce",
        };
    }

    GuardAction::None
}

/// Снижает ток на один шаг, не опускаясь ниже минимума.
#[must_use]
pub fn step_down(current_ua: u32, limits: &GuardLimits) -> u32 {
    let reduced = current_ua.saturating_sub(CURRENT_STEP_UA);
    if reduced < limits.iin_floor_ua {
        limits.iin_floor_ua
    } else {
        reduced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::OpMode;

    fn sample(iin_ua: u32, temp_dc: i32, vbat_uv: u32) -> TelemetrySample {
        TelemetrySample {
            ts_ms: 1_000,
            vbat_uv,
            vbus_uv: 9_000_000,
            iin_ua,
            die_temp_dc: temp_dc,
            op_mode: OpMode::Switching,
            input_present: true,
        }
    }

    #[test]
    fn normal_conditions_change_nothing() {
        let limits = GuardLimits::default();
        let action = evaluate(&sample(2_000_000, 350, 4_000_000), &limits);
        assert_eq!(action, GuardAction::None);
        assert!(!action.is_change());
    }

    #[test]
    fn overheating_steps_current_down() {
        let limits = GuardLimits::default();
        let action = evaluate(&sample(2_000_000, 440, 4_000_000), &limits);
        match action {
            GuardAction::ReduceCurrent { to_ua, reason } => {
                assert_eq!(reason, "die_temp_reduce");
                assert_eq!(to_ua, 1_750_000);
            }
            other => panic!("ожидалось снижение тока, получено {other:?}"),
        }
    }

    #[test]
    fn severe_heat_falls_back_to_bypass() {
        let limits = GuardLimits::default();
        let action = evaluate(&sample(2_000_000, 490, 4_000_000), &limits);
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
        let action = evaluate(&sample(2_000_000, 560, 4_000_000), &limits);
        assert_eq!(
            action,
            GuardAction::Stop {
                reason: "die_temp_stop"
            }
        );
    }

    #[test]
    fn overcurrent_is_capped_to_target() {
        let limits = GuardLimits::default();
        let action = evaluate(&sample(4_000_000, 350, 4_000_000), &limits);
        match action {
            GuardAction::ReduceCurrent { to_ua, reason } => {
                assert_eq!(reason, "iin_over_limit");
                assert_eq!(to_ua, limits.iin_target_ua);
            }
            other => panic!("ожидалось ограничение тока, получено {other:?}"),
        }
    }

    #[test]
    fn step_down_respects_floor() {
        let limits = GuardLimits::default();
        assert_eq!(step_down(600_000, &limits), 500_000);
        assert_eq!(step_down(500_000, &limits), 500_000);
        assert_eq!(step_down(2_000_000, &limits), 1_750_000);
    }

    #[test]
    fn rising_battery_voltage_reduces_current() {
        let limits = GuardLimits::default();
        let action = evaluate(&sample(1_500_000, 350, 4_430_000), &limits);
        match action {
            GuardAction::ReduceCurrent { reason, .. } => assert_eq!(reason, "vbat_reduce"),
            other => panic!("ожидалось снижение тока, получено {other:?}"),
        }
    }

    #[test]
    fn power_absent_does_nothing() {
        let limits = GuardLimits::default();
        let mut absent = sample(0, 900, 4_000_000);
        absent.input_present = false;
        assert_eq!(evaluate(&absent, &limits), GuardAction::None);
    }

    #[test]
    fn conservative_profile_is_stricter() {
        let strict = GuardLimits::conservative();
        let normal = GuardLimits::default();
        assert!(strict.temp_reduce_dc < normal.temp_reduce_dc);
        assert!(strict.iin_max_ua < normal.iin_max_ua);
        let action = evaluate(&sample(2_000_000, 420, 4_000_000), &strict);
        assert!(action.is_change(), "строгий профиль реагирует раньше");
    }
}
