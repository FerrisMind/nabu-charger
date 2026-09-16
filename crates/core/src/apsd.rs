//! Разбор результата аппаратной детекции адаптера (APSD).
//!
//! Логика повторяет функцию `smblib_get_apsd_result()` из эталонного драйвера
//! Android (`qcom/smb5-lib.c`, ветка 16.0, конфигурация `CONFIG_MACH_XIAOMI_NABU`),
//! чтобы под Windows тип адаптера определялся так же, как под Android.

use crate::error::ChargerError;
use crate::policy::Qc35Support;
use crate::regs;

/// Тип подключённого адаптера питания.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdapterType {
    /// Питание отсутствует или тип не определён.
    Unknown,
    /// Стандартный порт USB (предел 500 мА).
    Sdp,
    /// Прочий порт зарядки.
    Ocp,
    /// Порт зарядки с данными.
    Cdp,
    /// Порт только зарядки.
    Dcp,
    /// Нестандартный источник (FLOAT).
    Float,
    /// Quick Charge 2.0.
    Hvdcp2,
    /// Quick Charge 3.0.
    Hvdcp3,
    /// Quick Charge 3.5 (режим, который использует родной блок планшета).
    Hvdcp3P5,
}

impl AdapterType {
    /// Стабильное короткое имя для журнала и таблиц.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Sdp => "SDP",
            Self::Ocp => "OCP",
            Self::Cdp => "CDP",
            Self::Dcp => "DCP",
            Self::Float => "FLOAT",
            Self::Hvdcp2 => "HVDCP2",
            Self::Hvdcp3 => "HVDCP3",
            Self::Hvdcp3P5 => "HVDCP3P5",
        }
    }

    /// Является ли адаптер быстрым (Quick Charge).
    #[must_use]
    pub const fn is_hvdcp(self) -> bool {
        matches!(self, Self::Hvdcp2 | Self::Hvdcp3 | Self::Hvdcp3P5)
    }

    /// Образец, который аппаратура выставляет в `APSD_RESULT_STATUS`.
    ///
    /// Для [`AdapterType::Hvdcp3P5`] отдельного образца нет: аппаратура сообщает
    /// HVDCP3, а версия 3.5 подтверждается отдельной аутентификацией, поэтому
    /// возвращается образец HVDCP3.
    #[must_use]
    pub const fn apsd_pattern(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Sdp => regs::PATTERN_SDP,
            Self::Ocp => regs::PATTERN_OCP,
            Self::Cdp => regs::PATTERN_CDP,
            Self::Dcp => regs::PATTERN_DCP,
            Self::Float => regs::PATTERN_FLOAT,
            Self::Hvdcp2 => regs::PATTERN_HVDCP2,
            Self::Hvdcp3 | Self::Hvdcp3P5 => regs::PATTERN_HVDCP3,
        }
    }

    /// Разбирает образец без учёта признака Quick Charge.
    ///
    /// Возвращает [`ChargerError::UnknownAdapterPattern`], если сочетание битов
    /// не описано аппаратурой.
    ///
    /// # Errors
    ///
    /// [`ChargerError::UnknownAdapterPattern`] — если значение не совпало ни с
    /// одним известным образцом.
    pub fn from_pattern(raw: u8) -> Result<Self, ChargerError> {
        match raw {
            0 => Ok(Self::Unknown),
            regs::PATTERN_SDP => Ok(Self::Sdp),
            regs::PATTERN_OCP => Ok(Self::Ocp),
            regs::PATTERN_CDP => Ok(Self::Cdp),
            regs::PATTERN_DCP => Ok(Self::Dcp),
            regs::PATTERN_FLOAT => Ok(Self::Float),
            regs::PATTERN_HVDCP2 => Ok(Self::Hvdcp2),
            regs::PATTERN_HVDCP3 => Ok(Self::Hvdcp3),
            other => Err(ChargerError::UnknownAdapterPattern { raw: other }),
        }
    }

    /// Полный разбор пары регистров `APSD_STATUS` и `APSD_RESULT_STATUS`.
    ///
    /// Повторяет `smblib_get_apsd_result()`:
    ///
    /// 1. бит «детекция завершена» обязателен, иначе [`ChargerError::DetectionNotComplete`];
    /// 2. бит таймаута проверки HVDCP даёт [`ChargerError::AdapterCheckTimeout`];
    /// 3. образец из `APSD_RESULT_STATUS` переводится в базовый тип;
    /// 4. если выставлен бит Quick Charge, базовый тип уточняется: HVDCP3 остаётся
    ///    HVDCP3 (или становится HVDCP3P5 при подтверждённой аутентификации),
    ///    всё остальное считается HVDCP2.
    ///
    /// # Errors
    ///
    /// * [`ChargerError::DetectionNotComplete`] — детекция ещё идёт.
    /// * [`ChargerError::AdapterCheckTimeout`] — аппаратура зафиксировала таймаут HVDCP.
    /// * [`ChargerError::UnknownAdapterPattern`] — неизвестное сочетание битов.
    pub fn decode(
        apsd_status: u8,
        apsd_result: u8,
        qc35: Qc35Support,
    ) -> Result<Self, ChargerError> {
        if apsd_status & regs::APSD_DTC_STATUS_DONE == 0 {
            return Err(ChargerError::DetectionNotComplete);
        }
        if apsd_status & regs::HVDCP_CHECK_TIMEOUT != 0 {
            return Err(ChargerError::AdapterCheckTimeout {
                raw_status: apsd_status,
            });
        }

        let raw = apsd_result & regs::APSD_RESULT_STATUS_MASK;
        let base = Self::from_pattern(raw)?;
        if apsd_status & regs::QC_CHARGER == 0 {
            return Ok(base);
        }

        // QC3.5 подтверждается отдельной аутентификацией; без неё честно сообщаем HVDCP3.
        let resolved = match base {
            Self::Hvdcp3P5 => Self::Hvdcp3P5,
            Self::Hvdcp3 => {
                if matches!(
                    qc35,
                    Qc35Support::Supported {
                        authenticated: true
                    }
                ) {
                    Self::Hvdcp3P5
                } else {
                    Self::Hvdcp3
                }
            }
            // «since its a qc_charger, either return HVDCP3 or HVDCP2»
            _ => Self::Hvdcp2,
        };
        Ok(resolved)
    }

    /// Загружен ли порт зарядкой с точки зрения операционной системы.
    ///
    /// Любой тип, кроме [`AdapterType::Unknown`], означает присутствие источника.
    #[must_use]
    pub const fn is_power_present(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

impl core::fmt::Display for AdapterType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Qc35Support;

    #[test]
    fn decodes_base_patterns() {
        let done = regs::APSD_DTC_STATUS_DONE;
        let qc35 = Qc35Support::Unsupported;
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_SDP, qc35).expect("SDP"),
            AdapterType::Sdp
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_DCP, qc35).expect("DCP"),
            AdapterType::Dcp
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_CDP, qc35).expect("CDP"),
            AdapterType::Cdp
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_FLOAT, qc35).expect("FLOAT"),
            AdapterType::Float
        );
    }

    #[test]
    fn qc_bit_upgrades_type() {
        let done = regs::APSD_DTC_STATUS_DONE | regs::QC_CHARGER;
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_HVDCP2, Qc35Support::Unsupported)
                .expect("HVDCP2"),
            AdapterType::Hvdcp2
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_HVDCP3, Qc35Support::Unsupported)
                .expect("HVDCP3"),
            AdapterType::Hvdcp3
        );
        // Признак QC при неизвестном базовом типе трактуется как HVDCP2.
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_SDP, Qc35Support::Unsupported).expect("HVDCP2"),
            AdapterType::Hvdcp2
        );
    }

    #[test]
    fn qc35_authentication_promotes_type() {
        let done = regs::APSD_DTC_STATUS_DONE | regs::QC_CHARGER;
        assert_eq!(
            AdapterType::decode(
                done,
                regs::PATTERN_HVDCP3,
                Qc35Support::Supported {
                    authenticated: true
                }
            )
            .expect("HVDCP3.5"),
            AdapterType::Hvdcp3P5
        );
        assert_eq!(
            AdapterType::decode(
                done,
                regs::PATTERN_HVDCP3,
                Qc35Support::Supported {
                    authenticated: false
                }
            )
            .expect("HVDCP3"),
            AdapterType::Hvdcp3
        );
    }

    #[test]
    fn incomplete_detection_is_reported() {
        let err = AdapterType::decode(0, regs::PATTERN_DCP, Qc35Support::Unsupported)
            .expect_err("детекция не завершена");
        assert!(matches!(err, crate::ChargerError::DetectionNotComplete));
    }

    #[test]
    fn hvdcp_check_timeout_is_reported() {
        let status = regs::APSD_DTC_STATUS_DONE | regs::HVDCP_CHECK_TIMEOUT;
        let err = AdapterType::decode(status, regs::PATTERN_HVDCP3, Qc35Support::Unsupported)
            .expect_err("адаптер нестабилен");
        assert!(matches!(
            err,
            crate::ChargerError::AdapterCheckTimeout { .. }
        ));
    }

    #[test]
    fn unknown_pattern_is_reported() {
        let err = AdapterType::decode(regs::APSD_DTC_STATUS_DONE, 0x3F, Qc35Support::Unsupported)
            .expect_err("неизвестный образец");
        assert!(matches!(
            err,
            crate::ChargerError::UnknownAdapterPattern { raw: 0x3F }
        ));
    }

    #[test]
    fn helper_predicates_match_table() {
        assert!(AdapterType::Hvdcp3P5.is_hvdcp());
        assert!(!AdapterType::Dcp.is_hvdcp());
        assert!(AdapterType::Sdp.is_power_present());
        assert!(!AdapterType::Unknown.is_power_present());
        assert_eq!(AdapterType::Hvdcp3.apsd_pattern(), regs::PATTERN_HVDCP3);
        assert_eq!(AdapterType::Hvdcp3P5.label(), "HVDCP3P5");
    }
}
