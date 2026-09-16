//! Политика входного тока: сколько можно взять от распознанного адаптера.
//!
//! Константы взяты из эталонного драйвера Android
//! (`qcom/smb5-lib.h`, `ti/cp_qc30.h`, ветка 16.0). Токи — в микроамперax.

use crate::apsd::AdapterType;

/// Минимальный лимит входного тока (`DCIN_ICL_MIN_UA` в Android).
pub const MIN_ICL_UA: u32 = 100_000;

/// Верхняя граница, которую ядро разрешает выставить (`DCIN_ICL_MAX_UA` плюс запас).
pub const MAX_ICL_UA: u32 = 4_500_000;

/// Ток стандартного порта USB 2.0/3.x — 500 мА.
pub const ICL_SDP_UA: u32 = 500_000;
/// Ток порта зарядки по BC1.2 — 1.5 А.
pub const ICL_DCP_UA: u32 = 1_500_000;
/// Ток порта зарядки с данными по BC1.2 — 1.5 А.
pub const ICL_CDP_UA: u32 = 1_500_000;
/// Ток для HVDCP2 (`HVDCP2_CURRENT_UA`).
pub const ICL_HVDCP2_UA: u32 = 1_500_000;
/// Ток для HVDCP3 (`HVDCP_CURRENT_UA`).
pub const ICL_HVDCP3_UA: u32 = 3_000_000;
/// Ток для HVDCP3.5 на стороне SMB (ограничение одного ключа).
pub const ICL_HVDCP3P5_SMB_UA: u32 = 3_000_000;
/// Паспортный ток родного блока при работе через charge pump (`HVDCP3P5_40W_CURRENT_UA`).
pub const HVDCP3P5_PUMP_BUS_UA: u32 = 4_500_000;

/// Напряжение, запрашиваемое у адаптера в режиме QC2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc2Voltage {
    /// 5 В.
    V5,
    /// 9 В.
    V9,
    /// 12 В.
    V12,
}

impl Qc2Voltage {
    /// Значение битов 7:6 регистра `HVDCP_PULSE_COUNT_MAX`.
    #[must_use]
    pub const fn raw(self) -> u8 {
        match self {
            Self::V5 => 0x00,
            Self::V9 => 0x40,
            Self::V12 => 0x80,
        }
    }

    /// Подпись для журнала.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::V5 => "5V",
            Self::V9 => "9V",
            Self::V12 => "12V",
        }
    }
}

/// Состояние поддержки Quick Charge 3.5 на этой платформе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc35Support {
    /// Платформа не поддерживает QC3.5, тип понижается до HVDCP3/HVDCP2.
    Unsupported,
    /// Платформа поддерживает QC3.5; `authenticated` — прошла ли аутентификация.
    Supported {
        /// Результат аутентификации QC3.5.
        authenticated: bool,
    },
}

impl Default for Qc35Support {
    fn default() -> Self {
        // Планшет nabu поддерживает QC3.5, но аутентификация по умолчанию не пройдена.
        Self::Supported {
            authenticated: false,
        }
    }
}

/// Итоговое решение по распознанному адаптеру.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargePolicy {
    /// Лимит входного тока в микроамперax.
    pub icl_ua: u32,
    /// Требуется ли поднять напряжение у адаптера (QC2).
    pub qc2_voltage: Option<Qc2Voltage>,
    /// Можно ли подключать charge pump на вторую ступень.
    pub pump_eligible: bool,
    /// Обоснование для журнала.
    pub rationale: &'static str,
}

/// Считает политику по типу адаптера.
///
/// Для неизвестного источника выставляется минимальный безопасный ток: лучше
/// медленная зарядка, чем перегрузка порта.
#[must_use]
pub fn policy_for(adapter: AdapterType, qc35: Qc35Support) -> ChargePolicy {
    match adapter {
        AdapterType::Sdp => ChargePolicy {
            icl_ua: ICL_SDP_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "стандартный порт USB, предел 500 мА",
        },
        AdapterType::Cdp => ChargePolicy {
            icl_ua: ICL_CDP_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "порт зарядки с данными, BC1.2 1.5 А",
        },
        AdapterType::Dcp => ChargePolicy {
            icl_ua: ICL_DCP_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "порт только зарядки, BC1.2 1.5 А",
        },
        AdapterType::Ocp | AdapterType::Float | AdapterType::Unknown => ChargePolicy {
            icl_ua: MIN_ICL_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "источник не опознан, минимальный безопасный ток",
        },
        AdapterType::Hvdcp2 => ChargePolicy {
            icl_ua: ICL_HVDCP2_UA,
            qc2_voltage: Some(Qc2Voltage::V9),
            pump_eligible: false,
            rationale: "Quick Charge 2.0, 9 В и 1.5 А",
        },
        AdapterType::Hvdcp3 => ChargePolicy {
            icl_ua: ICL_HVDCP3_UA,
            qc2_voltage: Some(Qc2Voltage::V9),
            pump_eligible: true,
            rationale: "Quick Charge 3.0, 9 В и 3 А, возможен charge pump",
        },
        AdapterType::Hvdcp3P5 => {
            let pump_eligible = matches!(
                qc35,
                Qc35Support::Supported {
                    authenticated: true
                }
            );
            ChargePolicy {
                icl_ua: ICL_HVDCP3P5_SMB_UA,
                qc2_voltage: Some(Qc2Voltage::V9),
                pump_eligible,
                rationale: "Quick Charge 3.5, родной блок планшета",
            }
        }
    }
}

/// Ограничивает лимит тока сверху значениями из конфигурации ядра.
///
/// # Errors
///
/// [`crate::ChargerError::CurrentOutOfRange`] — если ток ниже минимума или выше
/// разрешённого максимума.
pub fn clamp_icl(icl_ua: u32, max_ua: u32) -> Result<u32, crate::error::ChargerError> {
    if icl_ua < MIN_ICL_UA {
        return Err(crate::error::ChargerError::CurrentOutOfRange {
            requested_ua: icl_ua,
            max_ua,
        });
    }
    if icl_ua > max_ua {
        return Err(crate::error::ChargerError::CurrentOutOfRange {
            requested_ua: icl_ua,
            max_ua,
        });
    }
    Ok(icl_ua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apsd::AdapterType;

    #[test]
    fn policy_table_matches_reference_driver() {
        let qc35 = Qc35Support::Unsupported;
        assert_eq!(policy_for(AdapterType::Sdp, qc35).icl_ua, ICL_SDP_UA);
        assert_eq!(policy_for(AdapterType::Cdp, qc35).icl_ua, ICL_CDP_UA);
        assert_eq!(policy_for(AdapterType::Dcp, qc35).icl_ua, ICL_DCP_UA);
        assert_eq!(policy_for(AdapterType::Hvdcp2, qc35).icl_ua, ICL_HVDCP2_UA);
        assert_eq!(policy_for(AdapterType::Hvdcp3, qc35).icl_ua, ICL_HVDCP3_UA);
    }

    #[test]
    fn unknown_source_gets_minimal_current() {
        let policy = policy_for(AdapterType::Unknown, Qc35Support::default());
        assert_eq!(policy.icl_ua, MIN_ICL_UA);
        assert!(policy.qc2_voltage.is_none());
        assert!(!policy.pump_eligible);
    }

    #[test]
    fn hvdcp_requests_nine_volts() {
        let policy = policy_for(AdapterType::Hvdcp3, Qc35Support::default());
        assert_eq!(policy.qc2_voltage, Some(Qc2Voltage::V9));
        assert_eq!(Qc2Voltage::V9.raw(), 0x40);
        assert_eq!(Qc2Voltage::V12.raw(), 0x80);
        assert_eq!(Qc2Voltage::V5.raw(), 0x00);
    }

    #[test]
    fn pump_requires_authentication_for_qc35() {
        let authenticated = policy_for(
            AdapterType::Hvdcp3P5,
            Qc35Support::Supported {
                authenticated: true,
            },
        );
        let plain = policy_for(
            AdapterType::Hvdcp3P5,
            Qc35Support::Supported {
                authenticated: false,
            },
        );
        assert!(authenticated.pump_eligible);
        assert!(!plain.pump_eligible);
    }

    #[test]
    fn clamp_rejects_out_of_range() {
        assert!(clamp_icl(500_000, MAX_ICL_UA).is_ok());
        assert!(clamp_icl(50_000, MAX_ICL_UA).is_err());
        assert!(clamp_icl(MAX_ICL_UA.saturating_add(1), MAX_ICL_UA).is_err());
    }
}
