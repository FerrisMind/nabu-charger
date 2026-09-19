//! Сценарий нагрева от холодного состояния до останова: все три уровня защиты.
//!
//! Тест повторяет путь драйвера: читает температуру из АЦП, принимает решение
//! защитой и **применяет его к чипу**, после чего проверяет регистры. Это не
//! проверка одной формулы, а сверка всей цепочки «датчик → решение → запись».
//!
//! Уход в 1:1 разрешает только Vin в окне обхода: на повышенном входе защита
//! снижает ток, а затем останавливает заряд (проверяется отдельным сценарием).
//!
//! Ступени берутся от порогов профиля (`GuardLimits::standard`), а не зашиты
//! числами: пороги обязаны лежать выше температуры покоя кристалла этой платы
//! (живой замер 19.09 — 46,1 °C в простое), и зашитые значения разъезжались с
//! ними молча.
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

/// Записывает температуру кристалла в регистры АЦП так, как это сделал бы чип.
///
/// Температура задаётся кодом канала: dC = (935 - code) * 4350 / 1000,
fn set_die_temp(pump: &mut Pump<MockPumpBus>, deci_celsius: i32) {
    // Обратное преобразование: code = 935 - (dC * 1000) / 4350.
    // то есть raw = 935 - (dC * 1000) / 4350. Значение ограничиваем диапазоном АЦП.
    let code = (935_i32 - (deci_celsius * 1000) / 4350).clamp(0, 1_023);
    let register = AdcChannel::DieTemp.register();
    let bus = pump.bus_mut();
    // Код канала упакован в общий поток бит: 6 бит кода лежат в младшем
    // байте со сдвигом 2, старшие 4 бита - в младших битах следующего.
    bus.set_reg(register, u8::try_from((code % 64) * 4).unwrap_or(0));
    bus.set_reg(register + 1, u8::try_from((code / 64) & 0x0F).unwrap_or(0));
}

/// Собирает отсчёт телеметрии: температура читается с чипа, остальные каналы —
/// фиксированные и правдоподобные.
///
/// Так сделано намеренно: пары регистров АЦП перекрываются (напряжение батареи
/// читается из `0x0E–0x0F`, температура из `0x0F–0x10`), поэтому подмена всех
/// каналов через мок дала бы мусорный отсчёт. Сценарий проверяет температуру, а ток
/// и напряжение держит в нормальном диапазоне, чтобы вмешивалась именно она.
fn sample_from(pump: &mut Pump<MockPumpBus>, now_ms: u64) -> TelemetrySample {
    sample_from_with_vin(pump, now_ms, 9_000_000)
}

/// [`sample_from`] с явным Vin: гейт обхода 1:1 зависит от входного напряжения.
fn sample_from_with_vin(pump: &mut Pump<MockPumpBus>, now_ms: u64, vin_uv: u32) -> TelemetrySample {
    let temp = pump
        .read_adc(AdcChannel::DieTemp)
        .expect("температура кристалла");
    TelemetrySample {
        ts_ms: now_ms,
        vbat_uv: 3_900_000,
        vbus_uv: vin_uv,
        iin_ua: 2_000_000,
        die_temp_dc: temp,
        op_mode: pump.op_mode(),
        input_present: true,
        // Температура только что прочитана с чипа; VBAT в этом сценарии —
        // правдоподобная константа, а не результат чтения АЦП.
        vbat_valid: true,
        die_temp_valid: true,
    }
}

/// Перегрев при повышенном Vin (живой сценарий QC/PD, 9 В).
///
/// 1:1 здесь — это 9 В на батарею, поэтому защита обязана вместо обхода снижать
/// ток, а при упорной температуре — останавливать заряд. Бит `EN_1TO1` не должен
/// появиться ни на одном шаге.
#[test]
fn thermal_ramp_on_elevated_vin_never_uses_bypass() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("открытие чипа");
    pump.configure().expect("настройка порогов");
    assert_eq!(
        pump.enable_switching().expect("режим 2:1"),
        OpMode::Switching
    );

    // Снимок профильного лимита — как в `read_parameters`: защита возвращает ток
    // именно к нему, и без снимка она бы «возвращала» к дефолтным 2 А.
    let mut limits = GuardLimits::standard();
    let target_before = pump.config().iin_limit_ua;
    limits.iin_profile_ua = target_before;

    // Температура растёт: норма → снижение тока → отказ 1:1 (снижение) → останов.
    // Ступени выводятся из порогов профиля: зашитые числа разъезжались с ними
    // молча (пороги подняты над температурой покоя кристалла — 46,1 °C).
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
        // АЦП квантует значение, поэтому сверяем с допуском, а не точно.
        assert!(
            (sample.die_temp_dc - temperature).abs() <= 5,
            "прочитанная температура {} должна быть близка к записанной {}",
            sample.die_temp_dc,
            temperature
        );

        // Намеренной уставки (тапера) в этом сценарии нет: Vbat далеко от полосы.
        let action = evaluate(
            &sample,
            &limits,
            pump.applied_iin_ua(),
            pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, false),
        );
        match action {
            GuardAction::None => {
                denied_strikes = 0;
                seen.push("норма");
            }
            GuardAction::ReduceCurrent { to_ua, .. } => {
                denied_strikes = 0;
                seen.push("снижение тока");
                pump.set_iin_limit(to_ua).expect("снижение тока в чипе");
            }
            GuardAction::FallbackToBypass { .. } => {
                // Ровно то, что делает KMDF: решение разрешается с учётом Vin.
                match resolve_bypass(
                    i32::try_from(sample.vbus_uv).expect("Vin в i32"),
                    sample.vbat_uv,
                    denied_strikes,
                    &limits,
                ) {
                    BypassResolution::Allowed => panic!("1:1 при 9 В недопустим"),
                    BypassResolution::ReduceCurrent { to_ua, .. } => {
                        denied_strikes = denied_strikes.saturating_add(1);
                        seen.push("отказ 1:1 → снижение тока");
                        pump.set_iin_limit(to_ua).expect("снижение тока в чипе");
                    }
                    BypassResolution::Stop { .. } => {
                        seen.push("отказ 1:1 → останов");
                        pump.standby().expect("останов заряда");
                    }
                    // Перечисление помечено `non_exhaustive`.
                    _ => seen.push("прочее"),
                }
            }
            GuardAction::Stop { .. } => {
                seen.push("останов");
                pump.standby().expect("останов заряда");
            }
            // Перечисление помечено `non_exhaustive`: новых вариантов защита пока не выдаёт.
            _ => seen.push("прочее"),
        }

        assert_eq!(
            pump.bus().reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "при повышенном Vin бит 1:1 не должен появляться (шаг {temperature})"
        );
    }

    assert_eq!(
        seen,
        vec![
            "норма",
            "снижение тока",
            "отказ 1:1 → снижение тока",
            "останов"
        ],
        "на повышенном Vin защита не уходит в 1:1, а режет ток и останавливается"
    );

    // Что реально произошло с чипом к концу сценария.
    let sys_ctrl = pump.bus().reg(regs::SYS_CTRL);
    assert_ne!(
        sys_ctrl & (1 << 3),
        0,
        "к концу сценария в SYS_CTRL должен стоять бит standby"
    );
    assert_eq!(
        pump.op_mode(),
        OpMode::Standby,
        "после останова ожидается режим standby"
    );
    assert!(
        pump.config().iin_limit_ua <= target_before,
        "лимит тока не должен вырасти от действий защиты"
    );
}

/// Кладёт в регистры АЦП Vin такое значение, чтобы `read_adc(Vin)` вернул ≈ `uv`.
fn set_vin_adc(pump: &mut Pump<MockPumpBus>, uv: i32) {
    let units = u16::try_from((uv / 16_000).clamp(0, 1023)).unwrap_or(0);
    let high = u8::try_from((units / 16) & 0x3F).unwrap_or(0);
    let low = u8::try_from((units % 16) * 16).unwrap_or(0);
    let register = AdcChannel::Vin.register();
    pump.bus_mut().set_reg(register, low);
    pump.bus_mut().set_reg(register + 1, high);
}

/// Тот же перегрев, но вход в окне обхода (5 В): 1:1 разрешён и включается.
#[test]
fn thermal_bypass_on_five_volts_is_allowed() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("открытие чипа");
    pump.configure().expect("настройка порогов");
    pump.enable_switching().expect("режим 2:1");

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
    pump.enable_bypass().expect("уход в bypass на 5 В");
    assert_eq!(
        pump.bus().reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
        regs::SYS_CTRL_EN_1TO1,
        "на 5 В обход обязан включаться"
    );
}

#[test]
fn cooled_down_chip_returns_to_switching() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("открытие чипа");
    pump.configure().expect("настройка");
    pump.enable_switching().expect("режим 2:1");

    let mut limits = GuardLimits::standard();
    limits.iin_profile_ua = pump.config().iin_limit_ua;

    // Сначала перегрев: уходим в защиту.
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
    pump.standby().expect("останов");

    // Затем остывание: чип можно вернуть в рабочий режим.
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
        "после остывания защита не должна вмешиваться"
    );
    pump.configure().expect("повторная настройка");
    assert_eq!(
        pump.enable_switching().expect("возврат в 2:1"),
        OpMode::Switching,
        "после остывания режим должен восстанавливаться"
    );
}
