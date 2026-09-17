//! Сценарий нагрева от холодного состояния до останова: все три уровня защиты.
//!
//! Тест повторяет путь драйвера: читает температуру из АЦП, принимает решение
//! защитой и **применяет его к чипу**, после чего проверяет регистры. Это не
//! проверка одной формулы, а сверка всей цепочки «датчик → решение → запись».
//!
//! Значения температур выбраны с запасом от порогов, чтобы тест не зависел от
//! округления при кодировании: 39.8 °C — норма, 44.1 °C — снижение тока,
//! 48.9 °C — уход в bypass, 55.9 °C — останов.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use ln8000::testkit::MockPumpBus;
use ln8000::{
    AdcChannel, GuardAction, GuardLimits, OpMode, Pump, PumpConfig, TelemetrySample, evaluate, regs,
};

/// Записывает температуру кристалла в регистры АЦП так, как это сделал бы чип.
///
/// Кодирование обратно формуле эталонного драйвера: `dC = raw * 4350 / 1000 - 250`.
fn set_die_temp(pump: &mut Pump<MockPumpBus>, deci_celsius: i32) {
    // Обратное преобразование к формуле эталона: (935 - raw) * 4350 / 1000,
    // то есть raw = 935 - (dC * 1000) / 4350. Значение ограничиваем диапазоном АЦП.
    let raw = (935_i32 - (deci_celsius * 1000) / 4350).clamp(0, 65_535);
    let raw = u16::try_from(raw).unwrap_or(0);
    let register = AdcChannel::DieTemp.register();
    let bus = pump.bus_mut();
    bus.set_reg(register, u8::try_from(raw & 0x00FF).unwrap_or(0));
    bus.set_reg(register + 1, u8::try_from(raw >> 8).unwrap_or(0));
}

/// Собирает отсчёт телеметрии: температура читается с чипа, остальные каналы —
/// фиксированные и правдоподобные.
///
/// Так сделано намеренно: пары регистров АЦП перекрываются (напряжение батареи
/// читается из `0x0E–0x0F`, температура из `0x0F–0x10`), поэтому подмена всех
/// каналов через мок дала бы мусорный отсчёт. Сценарий проверяет температуру, а ток
/// и напряжение держит в нормальном диапазоне, чтобы вмешивалась именно она.
fn sample_from(pump: &mut Pump<MockPumpBus>, now_ms: u64) -> TelemetrySample {
    let temp = pump
        .read_adc(AdcChannel::DieTemp)
        .expect("температура кристалла");
    TelemetrySample {
        ts_ms: now_ms,
        vbat_uv: 3_900_000,
        vbus_uv: 9_000_000,
        iin_ua: 2_000_000,
        die_temp_dc: temp,
        op_mode: pump.op_mode(),
        input_present: true,
    }
}

#[test]
fn thermal_ramp_walks_all_three_levels_and_changes_the_chip() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("открытие чипа");
    pump.configure().expect("настройка порогов");
    assert_eq!(
        pump.enable_switching().expect("режим 2:1"),
        OpMode::Switching
    );

    let limits = GuardLimits::standard();
    let target_before = pump.config().iin_limit_ua;

    // Температура растёт: норма → снижение тока → bypass → останов.
    let ramp = [398, 441, 489, 559];
    let mut seen: Vec<&'static str> = Vec::new();

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

        let action = evaluate(&sample, &limits);
        match action {
            GuardAction::None => seen.push("норма"),
            GuardAction::ReduceCurrent { to_ua, .. } => {
                seen.push("снижение тока");
                pump.set_iin_limit(to_ua).expect("снижение тока в чипе");
            }
            GuardAction::FallbackToBypass { .. } => {
                seen.push("bypass");
                pump.enable_bypass().expect("уход в bypass");
            }
            GuardAction::Stop { .. } => {
                seen.push("останов");
                pump.standby().expect("останов заряда");
            }
            // Перечисление помечено `non_exhaustive`: новых вариантов защита пока не выдаёт.
            _ => seen.push("прочее"),
        }
    }

    assert_eq!(
        seen,
        vec!["норма", "снижение тока", "bypass", "останов"],
        "уровни защиты должны срабатывать по возрастанию температуры"
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

#[test]
fn cooled_down_chip_returns_to_switching() {
    let bus = MockPumpBus::new();
    let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).expect("открытие чипа");
    pump.configure().expect("настройка");
    pump.enable_switching().expect("режим 2:1");

    let limits = GuardLimits::standard();

    // Сначала перегрев: уходим в защиту.
    set_die_temp(&mut pump, 559);
    let hot = sample_from(&mut pump, 0);
    assert!(matches!(evaluate(&hot, &limits), GuardAction::Stop { .. }));
    pump.standby().expect("останов");

    // Затем остывание: чип можно вернуть в рабочий режим.
    set_die_temp(&mut pump, 350);
    let cold = sample_from(&mut pump, 1_000);
    assert!(
        matches!(evaluate(&cold, &limits), GuardAction::None),
        "после остывания защита не должна вмешиваться"
    );
    pump.configure().expect("повторная настройка");
    assert_eq!(
        pump.enable_switching().expect("возврат в 2:1"),
        OpMode::Switching,
        "после остывания режим должен восстанавливаться"
    );
}
