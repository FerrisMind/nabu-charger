//! Защита по температуре и току: решение, что делать с режимом зарядки.
//!
//! Драйвер обязан не только включать ускоренный режим, но и вовремя его
//! ограничивать. Модуль принимает последний отсчёт телеметрии и лимиты, а
//! возвращает действие: ничего не делать, снизить ток, уйти в bypass или
//! полностью прекратить заряд.
//!
//! Пороги задаются в одних единицах с телеметрией: температура — десятые доли
//! °C, ток — микроамперы, напряжение — микровольты.

use crate::encoding::{bypass_allowed_by_vin, vbat_tracks_converter_rail};
use crate::session::TelemetrySample;

/// Гистерезис возврата тока по напряжению батареи, мкВ.
///
/// Пока Vbat не ушло ниже `vbat_reduce_uv − VBAT_REDUCE_HYST_UV`, уставка
/// держится: полоса 50 мВ не даёт защите «дрожать» между срезом и возвратом
/// на границе порога.
pub const VBAT_REDUCE_HYST_UV: u32 = 50_000;

/// Гистерезис возврата тока по температуре кристалла, десятые доли °C (3,0 °C).
pub const TEMP_REDUCE_HYST_DC: i32 = 30;

/// Верхняя граница правдоподобной температуры кристалла, десятые доли °C.
///
/// Шкала АЦП обрезана на +160,0 °C, и туда же попадает **нулевой сырой код**
/// (отказ канала: `AdcChannel::DieTemp.decode(0) == 1600`). Всё выше 125,0 °C
/// считается недостоверным: рабочий кристалл до такого не греется, а
/// предохранитель [`GuardLimits::temp_stop_dc`] (55,0 °C) лежит заметно ниже
/// границы, поэтому по честному отсчёту работает как раньше.
pub const DIE_TEMP_MAX_PLAUSIBLE_DC: i32 = 1_250;

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
    /// Профильный лимит входного тока насоса, мкА.
    ///
    /// Снимок `PumpConfig::iin_limit_ua` на момент настройки: к нему защита
    /// возвращает ток, когда напряжение и температура уходят из полосы среза.
    /// Сам `config.iin_limit_ua` для этого не годится — `Pump::set_iin_limit`
    /// перезаписывает его каждой уставкой, и «профильное» значение теряется.
    /// По умолчанию равен [`Self::iin_target_ua`]: если снимок не сделан,
    /// возврат не поднимет ток выше цели.
    pub iin_profile_ua: u32,
    /// Напряжение батареи, при котором снижаем ток (мкВ).
    ///
    /// Это порог **полного заряда**, а не петля QC3: для NABU не-FFC цель —
    /// 4,45 В (`qcom,non-fcc-fv-max-uv`), FFC — 4,47 В (`qcom,fv-max-uv`,
    /// dtbo-03). 4,42 В (`mi,qc3-bat-volt-max`) — лимит петли QC3, резать по
    /// нему ток нельзя, иначе заряд тормозится задолго до полного.
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
            iin_profile_ua: 2_000_000,
            // FFC-порог заряда NABU (4,47 В = `qcom,fv-max-uv`): ток режется
            // только у самого верха, а не с 4,42 В (лимита петли QC3).
            vbat_reduce_uv: 4_470_000,
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
            iin_profile_ua: 1_000_000,
            vbat_reduce_uv: 4_350_000,
        }
    }

    /// Проверяет, что пороги заданы в разумном порядке.
    ///
    /// Порядок принципиален: снижение тока обязано наступать раньше ухода в bypass,
    /// а bypass — раньше останова. Нарушенный порядок означает защиту, которая либо
    /// не сработает вовремя, либо сразу оборвёт заряд. То же для токов:
    /// пол ≤ цель ≤ максимум.
    #[must_use]
    pub const fn is_consistent(&self) -> bool {
        self.temp_reduce_dc < self.temp_bypass_dc
            && self.temp_bypass_dc < self.temp_stop_dc
            && self.iin_floor_ua <= self.iin_target_ua
            && self.iin_target_ua <= self.iin_max_ua
    }

    /// Уставка полосы среза: `min(профильный лимит, цель)`, мкА.
    ///
    /// Пока Vbat у верха заряда или кристалл горячий, защита держит **одну и ту
    /// же** уставку вместо пошагового вычитания: повторные вызовы не «сползают»
    /// вниз к полу за считаные такты.
    #[must_use]
    pub const fn band_iin_ua(&self) -> u32 {
        if self.iin_profile_ua < self.iin_target_ua {
            self.iin_profile_ua
        } else {
            self.iin_target_ua
        }
    }

    /// Профильный лимит с оглядкой на границы защиты, мкА.
    ///
    /// Сюда ток возвращается, когда и напряжение, и температура ушли из полосы
    /// среза ниже порогов гистерезиса.
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

    /// Применяет параметр из реестра к порогам защиты.
    ///
    /// Значение вне диапазона **или нарушающее порядок порогов** отклоняется
    /// целиком: набор остаётся прежним, а не превращается в частично обновлённый.
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
    /// Вернуть лимит входного тока к профильному значению.
    ///
    /// Появляется только после того, как и напряжение, и температура ушли из
    /// полосы среза ниже порогов гистерезиса: без такого явного возврата срез
    /// оставался бы навсегда.
    RestoreCurrent {
        /// Профильный лимит, мкА.
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
            Self::RestoreCurrent { .. } => "restore_current",
            Self::FallbackToBypass { .. } => "fallback_bypass",
            Self::Stop { .. } => "stop",
        }
    }
}

/// Пригодна ли температура кристалла для решения по защите.
///
/// Канал должен быть прочитан ([`TelemetrySample::die_temp_valid`]), а значение —
/// правдоподобным ([`DIE_TEMP_MAX_PLAUSIBLE_DC`]). Отказ чтения даёт нулевой
/// сырой код, то есть +160,0 °C на выходе декодера, — по такому отсчёту нельзя
/// ни останавливать заряд, ни уходить в 1:1.
#[must_use]
pub const fn die_temp_usable(sample: &TelemetrySample) -> bool {
    sample.die_temp_valid && sample.die_temp_dc <= DIE_TEMP_MAX_PLAUSIBLE_DC
}

/// Закончился ли эпизод перегрева, после которого счётчик отказов 1:1 обнуляют.
///
/// Признак — достоверная температура ушла ниже [`GuardLimits::temp_bypass_dc`]
/// на [`TEMP_REDUCE_HYST_DC`]. Без сброса второй эпизод ≥ `temp_bypass_dc`
/// остановил бы заряд на первом же такте, минуя ступень снижения тока: счётчик
/// `denied_strikes` хранится между эпизодами. Недостоверный отсчёт сбросом не
/// считается — счётчик держится до честного замера.
#[must_use]
pub const fn bypass_strikes_expired(sample: &TelemetrySample, limits: &GuardLimits) -> bool {
    die_temp_usable(sample)
        && sample.die_temp_dc <= limits.temp_bypass_dc.saturating_sub(TEMP_REDUCE_HYST_DC)
}

/// Считает действие защиты по последнему отсчёту и текущей уставке тока.
///
/// `applied_iin_ua` — уставка, которая **фактически стоит в чипе** (драйвер
/// читает `IIN_CTRL`): защита возвращает действие, только если уставку
/// действительно надо менять. Профиль для этого не годится — тапер у верха
/// заряда пишет 1,2 А мимо профиля (`config.iin_limit_ua` остаётся 2,8 А), и
/// решение «по профилю» поднимало бы ток вместо снижения. `None` означает, что
/// регистр не прочитан: такт оставляем без решений о токе, а не подставляем
/// профиль. Это же снимает «дрожание»: пока условие среза держится, уставка
/// одна и та же ([`GuardLimits::band_iin_ua`]). Возврат к профильному лимиту —
/// [`GuardAction::RestoreCurrent`] после ухода **и** напряжения, **и**
/// температуры ниже порогов гистерезиса.
///
/// `deliberate_iin_ua` — **намеренная** уставка владельца, который сейчас ведёт
/// ток помимо полос защиты ([`crate::Pump::taper_setpoint_ua`] у верха заряда).
/// Возврат снимает только собственное снижение защиты: он поднимает лимит не
/// выше этой уставки и никогда его не опускает. Без неё (`None`) поведение
/// прежнее — к профилю. Без такого потолка защита и тапер спорят за один
/// регистр: в окне, где полосы пересекаются, возврат каждые 250 мс отменял бы
/// намеренные 1,2 А.
///
/// Недостоверный отсчёт (канал VBAT не прочитан, температура канала нет или она
/// неправдоподобна) не порождает решений о снижении и возврате: настоящий ноль
/// и отказ чтения по значению неразличимы, а ошибка в сторону подъёма тока
/// опаснее пропущенного среза. Остаются только пороги по достоверной
/// температуре — `temp_stop_dc` и `temp_bypass_dc`.
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

    // Пороги «стоп» и «1:1» от уставки тока не зависят, но требуют достоверной
    // температуры: по отказу канала останавливать заряд нельзя.
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

    // Ниже — решения о токе: нужны и уставка из чипа, и достоверные каналы, по
    // которым они принимаются (температура кристалла и напряжение батареи).
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

    // VBAT ≈ Vin/2 — не клетка, а середина switch-cap шины (документировано в
    // `encoding::vbat_tracks_converter_rail`): по такому отсчёту ни резать, ни
    // возвращать ток нельзя, он ничего не говорит о батарее.
    let vbat_credible = !vbat_tracks_converter_rail(sample.vbat_uv, sample.vbus_uv);

    if vbat_credible && sample.vbat_uv >= limits.vbat_reduce_uv {
        return cap(applied, limits.band_iin_ua(), "vbat_reduce");
    }

    // Ниже — только возврат к профилю, и лишь когда лимит взаправду занижен.
    // Намеренная уставка (тапер у верха заряда) — потолок возврата: защита
    // снимает только **своё** снижение и никогда не опускает лимит сама —
    // понижение оставляем путям среза и таперу.
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

/// Уставка полосы среза: пишем её, только если текущий лимит выше.
///
/// Ниже уставки лимит не поднимает: иначе защита отбирала бы снижение у более
/// строгого действия (например, у запрещённого по Vin 1:1, где лимит уходит на пол).
fn cap(applied_iin_ua: u32, to_ua: u32, reason: &'static str) -> GuardAction {
    if applied_iin_ua <= to_ua {
        GuardAction::None
    } else {
        GuardAction::ReduceCurrent { to_ua, reason }
    }
}

/// Сколько тактов подряд защита терпит перегрев, когда 1:1 запрещён по Vin.
///
/// `1` означает: первый такт с `FallbackToBypass` на повышенном Vin снижает ток,
/// второй — останавливает заряд. Ждать дольше нельзя: безопасное отступление
/// (1:1) недоступно, а температура выше `temp_bypass_dc`.
pub const BYPASS_DENIED_STRIKES_BEFORE_STOP: u32 = 1;

/// Что делать с требованием защиты уйти в 1:1, если Vin этого не допускает.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BypassResolution {
    /// Vin в окне обхода — 1:1 разрешён.
    Allowed,
    /// 1:1 запрещён: снижаем входной ток до минимума и ждём следующего отсчёта.
    ReduceCurrent {
        /// Новый лимит, мкА.
        to_ua: u32,
        /// Причина для журнала.
        reason: &'static str,
    },
    /// 1:1 запрещён и температура не спадает: прекращаем заряд.
    Stop {
        /// Причина для журнала.
        reason: &'static str,
    },
}

/// Разрешает конфликт «защита просит 1:1, а вход не в окне обхода».
///
/// `denied_strikes` — сколько раз подряд это уже случалось до текущего такта.
/// На повышенном Vin вместо 1:1 защита снижает ток, а если температура держится
/// дольше [`BYPASS_DENIED_STRIKES_BEFORE_STOP`] тактов — останавливает заряд.
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

    /// Вызов защиты с уставкой, которая уже стоит в чипе: так его зовёт драйвер
    /// (`Pump::applied_iin_ua`). Отдельная обёртка — чтобы тесты не пестрили
    /// `Some(...)` и проверяли именно логику полос. Намеренной уставки у этих
    /// сценариев нет (`None`): тапер их не ведёт.
    fn evaluate_applied(
        sample: &TelemetrySample,
        limits: &GuardLimits,
        applied_iin_ua: u32,
    ) -> GuardAction {
        evaluate(sample, limits, Some(applied_iin_ua), None)
    }

    /// Профиль как у `PumpConfig::for_qc35_class_b`: лимит насоса 2,8 А.
    ///
    /// Снимок профильного лимита делает вызывающий (`read_parameters`), поэтому
    /// в тестах его приходится проставлять руками: иначе полоса среза совпала бы
    /// с целью, и резать было бы нечего.
    fn profile_limits() -> GuardLimits {
        let mut limits = GuardLimits::standard();
        limits.iin_profile_ua = 2_800_000;
        // Не-FFC цель NABU — 4,45 В (`qcom,non-fcc-fv-max-uv`): именно она
        // записана в реестре устройства, FFC 4,47 В задаётся явным `VbatReduceUv`.
        limits.vbat_reduce_uv = crate::encoding::NABU_VBAT_NON_FFC_UV;
        limits
    }

    fn sample(iin_ua: u32, temp_dc: i32, vbat_uv: u32) -> TelemetrySample {
        TelemetrySample {
            ts_ms: 1_000,
            vbat_uv,
            // Живой Vin ускоренного заряда — 9,6 В: артефакт «VBAT ≈ Vin/2»
            // живёт в 4,72…4,88 В и рабочих порогов заряда не касается.
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
        assert!(limits.apply_parameter("TempReduceDc", 420));
        assert_eq!(limits.temp_reduce_dc, 420);
        assert!(limits.apply_parameter("TempStopDc", 560));
        assert_eq!(limits.temp_stop_dc, 560);
        assert!(limits.apply_parameter("IinTargetUa", 2_500_000));
        assert_eq!(limits.iin_target_ua, 2_500_000);
        assert!(limits.apply_parameter("IinFloorUa", 1_000_000));
        assert!(limits.apply_parameter("VbatReduceUv", 4_300_000));
        assert!(
            limits.is_consistent(),
            "набор должен остаться согласованным"
        );
    }

    #[test]
    fn limits_reject_broken_order_and_junk() {
        let mut limits = GuardLimits::standard();
        let before = limits;

        // Снижение тока позже ухода в bypass — защита перестала бы работать по порядку.
        assert!(!limits.apply_parameter("TempReduceDc", 490));
        // Цель ниже полу — несогласованные токи.
        assert!(!limits.apply_parameter("IinTargetUa", 100_000));
        // Значения вне диапазонов и чужие имена.
        assert!(!limits.apply_parameter("TempStopDc", 10_000));
        assert!(!limits.apply_parameter("IinMaxUa", 10));
        assert!(!limits.apply_parameter("ТакогоПорогаНет", 1));

        assert_eq!(
            limits, before,
            "отклонённые значения не должны менять набор порогов частично"
        );
    }

    #[test]
    fn band_and_restore_limits_stay_inside_the_profile_bounds() {
        // Профиль выше предела защиты: возврат не должен пробить максимум.
        let mut limits = GuardLimits::standard();
        limits.iin_profile_ua = 4_000_000;
        assert_eq!(limits.restore_iin_ua(), limits.iin_max_ua);
        assert_eq!(limits.band_iin_ua(), limits.iin_target_ua);
        // Профиль ниже пола: возврат не должен уйти под минимум.
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
        let action = evaluate_applied(
            &sample(2_800_000, 440, 4_000_000),
            &limits,
            limits.restore_iin_ua(),
        );
        match action {
            GuardAction::ReduceCurrent { to_ua, reason } => {
                assert_eq!(reason, "die_temp_reduce");
                assert_eq!(to_ua, limits.band_iin_ua());
                assert_eq!(to_ua, 2_000_000);
            }
            other => panic!("ожидалось ограничение тока, получено {other:?}"),
        }
        // Применённая уставка повторно не пишется.
        assert_eq!(
            evaluate_applied(&sample(2_800_000, 440, 4_000_000), &limits, 2_000_000),
            GuardAction::None
        );
    }

    #[test]
    fn severe_heat_falls_back_to_bypass() {
        let limits = GuardLimits::default();
        let action = evaluate_applied(
            &sample(2_000_000, 490, 4_000_000),
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
            &sample(2_000_000, 560, 4_000_000),
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
            other => panic!("ожидалось ограничение тока, получено {other:?}"),
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
            "у верха заряда уставка — полоса, а не шаг вниз от текущего тока"
        );
        // Двадцать тактов подряд: уставка не «ползёт» вниз (в прошлой ревизии
        // она за девять тактов уходила на пол 500 мА и там оставалась).
        for tick in 0..20 {
            assert_eq!(
                evaluate_applied(&near_full, &limits, applied),
                first,
                "такт {tick}: уставка обязана быть той же"
            );
        }
        // Более строгое действие (пол после запрещённого 1:1) не отменяем.
        assert_eq!(
            evaluate_applied(&near_full, &limits, limits.iin_floor_ua),
            GuardAction::None
        );
        // 4,44 В — внутри полосы гистерезиса (порог возврата 4,40 В): держим.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 350, 4_440_000), &limits, 2_000_000),
            GuardAction::None
        );
        // 4,40 В = 4,45 − 0,05 — явный возврат к профильному лимиту.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 350, 4_400_000), &limits, 2_000_000),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );
        // Ушли из полосы, но лимит и так профильный — возвращать нечего, шину не трогаем.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 350, 4_000_000), &limits, 2_800_000),
            GuardAction::None
        );
    }

    #[test]
    fn die_temp_band_caps_without_creeping_and_restores_below_hysteresis() {
        let limits = profile_limits();
        let hot = sample(2_800_000, 431, 4_000_000);
        let capped = GuardAction::ReduceCurrent {
            to_ua: 2_000_000,
            reason: "die_temp_reduce",
        };
        assert_eq!(evaluate_applied(&hot, &limits, 2_800_000), capped);
        for tick in 0..20 {
            assert_eq!(
                evaluate_applied(&hot, &limits, 2_800_000),
                capped,
                "такт {tick}: уставка обязана быть той же"
            );
        }
        // 41,5 °C — внутри полосы гистерезиса (порог возврата 40,0 °C): держим.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 415, 4_000_000), &limits, 2_000_000),
            GuardAction::None
        );
        // 40,0 °C — возврат к профильному лимиту.
        assert_eq!(
            evaluate_applied(&sample(2_000_000, 400, 4_000_000), &limits, 2_000_000),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );
    }

    #[test]
    fn converter_rail_artifact_neither_reduces_nor_locks_the_limit() {
        let limits = profile_limits();
        // Живой случай: Vin 9616 мВ, VBAT 4780 мВ, ток 39 мА — это середина
        // switch-cap шины, а не клетка: резать по такому отсчёту нечего.
        let artifact = sample(39_000, 350, 4_780_000);
        assert_eq!(
            evaluate_applied(&artifact, &limits, limits.restore_iin_ua()),
            GuardAction::None
        );
        // Пока артефакт держится, лимит не должен запираться на полу: о батарее
        // отсчёт ничего не говорит, поэтому возврат к профилю разрешён.
        assert_eq!(
            evaluate_applied(&artifact, &limits, limits.iin_floor_ua),
            GuardAction::RestoreCurrent {
                to_ua: limits.restore_iin_ua(),
                reason: "reduce_band_exit",
            }
        );
        // Правдоподобный отсчёт на том же Vin режет ток как обычно.
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
        // 4,42 В — лимит петли QC3, а не порог полного заряда: по нему ток
        // резать нельзя. Не-FFC цель 4,45 В, FFC 4,47 В (dtbo-03).
        let limits = GuardLimits::standard();
        assert_eq!(limits.vbat_reduce_uv, crate::encoding::NABU_VBAT_FLOAT_UV);
        assert!(limits.vbat_reduce_uv > crate::encoding::NABU_QC3_BAT_VOLT_MAX_UV);

        let mut t = profile_limits();
        assert!(t.apply_parameter("VbatReduceUv", crate::encoding::NABU_VBAT_NON_FFC_UV));
        assert_eq!(t.vbat_reduce_uv, 4_450_000);

        let profile = t.restore_iin_ua();
        // Ниже порога охрана молчит; на пороге и выше — режет ток.
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
        // Строгий профиль — не трогаем без обоснования: он и должен резать рано.
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
        assert!(action.is_change(), "строгий профиль реагирует раньше");
        assert_eq!(
            evaluate_applied(
                &sample(2_000_000, 420, 4_000_000),
                &normal,
                normal.restore_iin_ua()
            ),
            GuardAction::None,
            "обычный профиль на 42,0 °C ещё молчит"
        );
    }

    #[test]
    fn bypass_resolution_is_allowed_only_on_the_five_volt_side() {
        let limits = GuardLimits::standard();
        // 5 В: 1:1 разрешён и защита не выдумывает обходных действий.
        assert_eq!(
            resolve_bypass(5_000_000, 4_000_000, 0, &limits),
            BypassResolution::Allowed
        );
        assert_eq!(
            resolve_bypass(7_999_999, 4_400_000, 5, &limits),
            BypassResolution::Allowed,
            "счётчик отказов не должен мешать законному обходу"
        );
        // 9 В: 1:1 — это 9 В на батарею. Первый такт — снижение тока до полу.
        assert_eq!(
            resolve_bypass(9_000_000, 4_275_000, 0, &limits),
            BypassResolution::ReduceCurrent {
                to_ua: limits.iin_floor_ua,
                reason: "die_temp_bypass_no_vin_headroom",
            }
        );
        // Температура не спадает — второй такт останавливает заряд.
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
        // 8,0 В — уже запрещено; вход ниже 4,2 В — тоже (1:1 бесполезен).
        for vin in [8_000_000, 8_416_000, 12_000_000] {
            assert!(
                !matches!(
                    resolve_bypass(vin, 4_200_000, 0, &limits),
                    BypassResolution::Allowed
                ),
                "Vin {vin} не должен разрешать 1:1"
            );
        }
        assert!(!matches!(
            resolve_bypass(4_000_000, 4_000_000, 0, &limits),
            BypassResolution::Allowed
        ));
    }
    #[test]
    fn taper_setpoint_is_never_raised_by_the_guard() {
        // F10: тапер у верха заряда пишет 1,2 А мимо профиля (в `config` остаётся
        // 2,8 А). Решение «по профилю» подняло бы ток до полосы 2,0 А, то есть
        // защита сработала бы в противоположную сторону.
        let limits = profile_limits();
        let tapered = sample(1_200_000, 440, 4_460_000);
        assert_eq!(
            evaluate(
                &tapered,
                &limits,
                Some(crate::encoding::VBAT_TAPER_IIN_UA),
                None
            ),
            GuardAction::None,
            "1,2 А ниже полосы среза: поднимать ток защита не имеет права"
        );
        // Уставка ещё ниже — тоже ничего: срез только снижает.
        assert_eq!(
            evaluate(&tapered, &limits, Some(1_000_000), None),
            GuardAction::None
        );
        // А с фактической уставкой выше полосы решение обычное — срез до 2,0 А.
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
        // F15: полоса тапера (Vbat >= 4,37 В при цели 4,47 В) пересекается с
        // полосой возврата (Vbat <= 4,40 В при пороге 4,45 В). В этом окне
        // защита, увидев заниженную уставку 1,2 А, возвращала профильные 2,8 А,
        // то есть отменяла намеренное снижение тапера каждые 250 мс.
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
            "низкий ток внутри полосы тапера намеренный: возврат не имеет права его поднимать"
        );

        // Контроль: без намеренной уставки (тапер не ведёт ток) возврат к
        // профилю обязан работать — потолок не должен отключать саму защиту.
        assert_eq!(
            evaluate(&taper, &limits, Some(1_200_000), None),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            },
            "защита без тапера возвращает профиль как раньше"
        );

        // И вне полосы тапера (Vbat ниже порога) возврат тоже срабатывает.
        let below_band = sample(1_200_000, 400, 4_200_000);
        assert_eq!(
            evaluate(&below_band, &limits, Some(1_200_000), None),
            GuardAction::RestoreCurrent {
                to_ua: 2_800_000,
                reason: "reduce_band_exit",
            }
        );

        // Потолок: возврат поднимает лимит только **до** намеренной уставки,
        // выше — нет, и сам лимит не опускает (понижение — дело путей среза).
        assert_eq!(
            evaluate(&below_band, &limits, Some(500_000), Some(1_200_000)),
            GuardAction::RestoreCurrent {
                to_ua: 1_200_000,
                reason: "reduce_band_exit",
            },
            "потолок возврата — намеренная уставка, а не профиль"
        );
        assert_eq!(
            evaluate(&below_band, &limits, Some(1_200_000), Some(1_200_000)),
            GuardAction::None,
            "на потолке возвращать нечего"
        );
    }

    #[test]
    fn unread_setpoint_defers_current_decisions() {
        // F10: регистр `IIN_CTRL` не прочитан — фактическая уставка неизвестна.
        // Профиль вместо неё не подставляем: решений о токе в этом такте нет,
        // но пороги по достоверной температуре продолжают работать.
        let limits = profile_limits();
        assert_eq!(
            evaluate(&sample(2_800_000, 440, 4_460_000), &limits, None, None),
            GuardAction::None,
            "срез без уставки не считаем"
        );
        assert_eq!(
            evaluate(&sample(4_000_000, 350, 4_000_000), &limits, None, None),
            GuardAction::None,
            "перегрузку по току без уставки тоже не режем"
        );
        assert_eq!(
            evaluate(&sample(2_800_000, 490, 4_000_000), &limits, None, None),
            GuardAction::FallbackToBypass {
                reason: "die_temp_bypass"
            },
            "порог 1:1 от уставки не зависит"
        );
        assert_eq!(
            evaluate(&sample(2_800_000, 560, 4_000_000), &limits, None, None),
            GuardAction::Stop {
                reason: "die_temp_stop"
            }
        );
    }

    #[test]
    fn invalid_channels_never_change_the_current() {
        // F11: отказ канала по значению неотличим от честного нуля, поэтому
        // невалидный такт не даёт ни среза, ни возврата — только `None`.
        let limits = profile_limits();
        let profile = limits.restore_iin_ua();

        // VBAT не прочитан (0 мкВ): ни возврата к профилю, ни среза.
        let mut bad_vbat = sample(2_000_000, 350, 0);
        bad_vbat.vbat_valid = false;
        assert_eq!(
            evaluate(&bad_vbat, &limits, Some(limits.iin_floor_ua), None),
            GuardAction::None,
            "по отказавшему каналу батареи возврат к профилю запрещён"
        );
        assert_eq!(
            evaluate(&bad_vbat, &limits, Some(profile), None),
            GuardAction::None,
            "и срез по нулю не выдумываем"
        );

        // DieTemp не прочитан, а значение — сырой ноль (декодер даёт +160,0 °C).
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

        // Канал прочитан, но значение неправдоподобно (`AdcChannel::DieTemp`
        // отдаёт 160,0 °C на нулевом коде): предохранитель по мусору не срабатывает.
        let garbage = sample(2_000_000, 1_600, 4_000_000);
        assert!(
            garbage.die_temp_valid,
            "канал при этом считается прочитанным"
        );
        assert!(!die_temp_usable(&garbage));
        assert_eq!(
            evaluate(&garbage, &limits, Some(profile), None),
            GuardAction::None,
            "недостоверная температура не останавливает заряд"
        );
        assert_eq!(
            evaluate(&garbage, &limits, Some(limits.iin_floor_ua), None),
            GuardAction::None
        );
    }

    #[test]
    fn valid_cold_tick_restores_the_profile_limit() {
        // F11, обратная сторона: оба канала живы и холодно — возврат работает,
        // предохранитель по настоящему нулю не отключён (0,0 °C ниже порогов).
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
        // Тот же отсчёт, но жарко — возврата нет, а уставка уходит в полосу.
        assert_eq!(
            evaluate(
                &sample(2_000_000, 440, 4_000_000),
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
        // F12: счётчик отказов 1:1 обнуляется на выходе из полосы. Без сброса
        // второй эпизод ≥ 48 °C остановил бы заряд на первом же такте, минуя
        // ступень снижения тока.
        let limits = profile_limits();
        assert!(
            !bypass_strikes_expired(&sample(2_800_000, 490, 4_250_000), &limits),
            "внутри полосы счётчик не сбрасывается"
        );
        // 47,0 °C — ещё в полосе гистерезиса (порог сброса 48,0 − 3,0 = 45,0 °C).
        assert!(!bypass_strikes_expired(
            &sample(2_000_000, 470, 4_250_000),
            &limits
        ));
        assert!(
            bypass_strikes_expired(&sample(2_000_000, 450, 4_250_000), &limits),
            "45,0 °C — выход из полосы перегрева"
        );
        // Недостоверная температура сбросом не считается.
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
                "эпизод {episode}: первый такт — ступень снижения тока"
            );
            denied_strikes += 1;
            assert_eq!(
                resolve_bypass(9_000_000, 4_250_000, denied_strikes, &limits),
                BypassResolution::Stop {
                    reason: "die_temp_bypass_no_vin_headroom",
                },
                "эпизод {episode}: второй такт — останов заряда"
            );
            // Так это делает KMDF перед решением защиты.
            if bypass_strikes_expired(&sample(2_000_000, 450, 4_250_000), &limits) {
                denied_strikes = 0;
            }
        }
        assert_eq!(
            denied_strikes, 0,
            "оба эпизода начинаются с нуля отказов и ведут себя одинаково"
        );
    }
}
