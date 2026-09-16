//! Паспорт драйвера: версия, железо, карта регистров, таблица политик.
//!
//! Используется командой `verify` и попадает в отчёт о приёмке, чтобы спецификация
//! была проверяемой, а не пересказанной.

use crate::apsd::AdapterType;
use crate::policy::{self, Qc35Support};

/// Версия крейта.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Модель устройства.
pub const MODEL: &str = "Xiaomi Pad 5 (nabu)";

/// Платформа.
pub const PLATFORM: &str = "Qualcomm SM8150 (Snapdragon 860), Windows on ARM64";

/// Зарядник в PMIC.
pub const CHARGER_BLOCK: &str = "PM8150B SMB (периферия USBIN, база 0x1300)";

/// Внешний charge pump.
pub const CHARGE_PUMP: &str = "LN8000, I2C 0x51, узел ACPI PEIC (QCOM057E)";

/// Один регистр из спецификации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterSpec {
    /// Адрес.
    pub addr: u16,
    /// Имя из документации.
    pub name: &'static str,
    /// Назначение.
    pub purpose: &'static str,
}

/// Регистры, с которыми работает драйвер.
pub const REGISTERS: &[RegisterSpec] = &[
    RegisterSpec {
        addr: crate::regs::APSD_STATUS,
        name: "APSD_STATUS",
        purpose: "состояние автомата детекции: готовность и признак Quick Charge",
    },
    RegisterSpec {
        addr: crate::regs::APSD_RESULT_STATUS,
        name: "APSD_RESULT_STATUS",
        purpose: "результат детекции: тип адаптера в битах 6:0",
    },
    RegisterSpec {
        addr: crate::regs::QC_CHANGE_STATUS,
        name: "QC_CHANGE_STATUS",
        purpose: "состояние переговоров Quick Charge",
    },
    RegisterSpec {
        addr: crate::regs::CMD_APSD,
        name: "CMD_APSD",
        purpose: "перезапуск детекции (бит APSD_RERUN)",
    },
    RegisterSpec {
        addr: crate::regs::CMD_ICL_OVERRIDE,
        name: "CMD_ICL_OVERRIDE",
        purpose: "разрешение принудительного лимита входного тока",
    },
    RegisterSpec {
        addr: crate::regs::USBIN_CURRENT_LIMIT_CFG,
        name: "USBIN_CURRENT_LIMIT_CFG",
        purpose: "код лимита входного тока (сетка 100 мА)",
    },
    RegisterSpec {
        addr: crate::regs::USBIN_ICL_OPTIONS,
        name: "USBIN_ICL_OPTIONS",
        purpose: "дополнительные опции лимита тока",
    },
    RegisterSpec {
        addr: crate::regs::HVDCP_PULSE_COUNT_MAX,
        name: "HVDCP_PULSE_COUNT_MAX",
        purpose: "выбор напряжения Quick Charge 2.0 (биты 7:6)",
    },
];

/// Строка таблицы политик.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterSpec {
    /// Тип адаптера.
    pub adapter: AdapterType,
    /// Образец в `APSD_RESULT_STATUS`.
    pub pattern: u8,
    /// Лимит входного тока по политике.
    pub icl_ua: u32,
    /// Допустим ли charge pump.
    pub pump_eligible: bool,
}

/// Таблица политик по умолчанию (QC3.5 без аутентификации).
#[must_use]
pub fn adapter_table() -> [AdapterSpec; 7] {
    let qc35 = Qc35Support::default();
    [
        AdapterType::Sdp,
        AdapterType::Cdp,
        AdapterType::Dcp,
        AdapterType::Float,
        AdapterType::Hvdcp2,
        AdapterType::Hvdcp3,
        AdapterType::Hvdcp3P5,
    ]
    .map(|adapter| {
        let policy = policy::policy_for(adapter, qc35);
        AdapterSpec {
            adapter,
            pattern: adapter.apsd_pattern(),
            icl_ua: policy.icl_ua,
            pump_eligible: policy.pump_eligible,
        }
    })
}

/// Текстовый паспорт драйвера.
#[cfg(feature = "std")]
#[must_use]
pub fn render_spec() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "драйвер     : core {VERSION}");
    let _ = writeln!(out, "устройство  : {MODEL}");
    let _ = writeln!(out, "платформа   : {PLATFORM}");
    let _ = writeln!(out, "зарядник    : {CHARGER_BLOCK}");
    let _ = writeln!(out, "charge pump : {CHARGE_PUMP}");
    let _ = writeln!(out, "\nрегистры:");
    for spec in REGISTERS {
        let _ = writeln!(
            out,
            "  0x{:04X}  {:<26} {}",
            spec.addr, spec.name, spec.purpose
        );
    }
    let _ = writeln!(out, "\nполитика тока по типам адаптера:");
    for spec in adapter_table() {
        let _ = writeln!(
            out,
            "  {:<9} образец 0x{:02X}  {:>8} мкА  pump: {}",
            spec.adapter.label(),
            spec.pattern,
            spec.icl_ua,
            if spec.pump_eligible { "да" } else { "нет" }
        );
    }
    out
}
