//! Разбор состояния LN8000: режим, защиты, отказы и показания АЦП.

use crate::encoding::OpMode;
use crate::regs;

/// Канал АЦП LN8000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdcChannel {
    /// Напряжение на выходе (5 мВ/LSB).
    Vout,
    /// Напряжение входа (16 мВ/LSB).
    Vin,
    /// Напряжение батареи (5 мВ/LSB).
    Vbat,
    /// Напряжение блока питания (16 мВ/LSB, смещение 5 LSB).
    Vac,
    /// Входной ток (4.89 мА/LSB).
    Iin,
    /// Температура кристалла (0.435 °C/LSB, смещение −25 °C).
    DieTemp,
    /// Термистор батареи (2.933 мВ/LSB).
    TsBat,
    /// Термистор шины (2.933 мВ/LSB).
    TsBus,
}

impl AdcChannel {
    /// Регистр результата канала.
    ///
    /// Соответствие взято из `switch (ch)` функции `ln8000_get_adc_data()`:
    /// VOUT ← ADC04, VIN ← ADC03, VBAT ← ADC06, VAC ← ADC02, IIN ← ADC01,
    /// DIETEMP ← ADC07, TSBAT ← ADC08, TSBUS ← ADC09. Код занимает два байта,
    /// поэтому каналы читаются парой от своего регистра.
    #[must_use]
    pub const fn register(self) -> u8 {
        regs::ADC_FIRST_STS.saturating_add(match self {
            // Номера каналов как в эталонном драйвере (`enum ln8000_adc_channel_index`):
            // VOUT=1, VIN=2, VBAT=3, VAC=4, IIN=5, DIETEMP=6, TSBAT=7, TSBUS=8.
            // Регистр ADC05 (0x0D) не используется ни одним каналом, поэтому
            // простым смещением от ADC01 обойтись нельзя: VBAT и далее стоят на
            // один регистр дальше.
            Self::Iin => 0,
            Self::Vac => 1,
            Self::Vin => 2,
            Self::Vout => 3,
            Self::Vbat => 5,
            Self::DieTemp => 6,
            Self::TsBat => 7,
            Self::TsBus => 8,
        })
    }

    /// Код канала, как в драйвере (`enum ln8000_adc_channel_index`).
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Vout => 1,
            Self::Vin => 2,
            Self::Vbat => 3,
            Self::Vac => 4,
            Self::Iin => 5,
            Self::DieTemp => 6,
            Self::TsBat => 7,
            Self::TsBus => 8,
        }
    }

    /// Имя канала для журнала.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Vout => "vout",
            Self::Vin => "vin",
            Self::Vbat => "vbat",
            Self::Vac => "vac",
            Self::Iin => "iin",
            Self::DieTemp => "die_temp",
            Self::TsBat => "ts_bat",
            Self::TsBus => "ts_bus",
        }
    }

    /// Все каналы в порядке перечисления драйвера.
    pub const ALL: [Self; 8] = [
        Self::Vout,
        Self::Vin,
        Self::Vbat,
        Self::Vac,
        Self::Iin,
        Self::DieTemp,
        Self::TsBat,
        Self::TsBus,
    ];

    /// Соответствие «канал → номер регистра АЦП», как в драйвере.
    #[must_use]
    pub const fn adc_index(self) -> u8 {
        match self {
            Self::Iin => 1,
            Self::Vac => 2,
            Self::Vin => 3,
            Self::Vout => 4,
            Self::Vbat => 6,
            Self::DieTemp => 7,
            Self::TsBat => 8,
            Self::TsBus => 9,
        }
    }

    /// Переводит код регистра в физическую величину.
    ///
    /// Код — 10-битный (результат АЦП занимает два регистра), поэтому
    /// принимается `u16`. Возвращает значение в микровольтах, микроамперax или
    /// десятых долях градуса — в зависимости от канала (см. [`AdcChannel`]).
    ///
    /// Арифметика без насыщения допустима: код не превышает 1023, а самый
    /// большой множитель — 16 000, то есть максимум ≈ 16.4 млн — с огромным
    /// запасом влезает в `i32`.
    #[must_use]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn decode(self, raw: u16) -> i32 {
        let code = i32::from(raw);
        match self {
            Self::Vout => code * 5_000,
            Self::Vin => code * 16_000,
            Self::Vbat => 1_000_000 + code * 5_000,
            Self::Vac => (code + 5) * 16_000,
            Self::Iin => code * 4_890,
            // Как в эталоне: (935 - raw) * 4350 / 1000, с ограничением [-250; 1600].
            Self::DieTemp => {
                let dc = (935_i32 - i32::from(code)) * 4_350 / 1_000;
                dc.clamp(-250, 1_600)
            }
            Self::TsBat | Self::TsBus => code * 2_933,
        }
    }

    /// Единица измерения значения [`AdcChannel::decode`].
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Vout | Self::Vin | Self::Vbat | Self::Vac | Self::TsBat | Self::TsBus => "uV",
            Self::Iin => "uA",
            Self::DieTemp => "dC",
        }
    }
}

/// Снимок состояния устройства.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Сырое значение `SYS_STS`.
    pub sys_sts: u8,
    /// Разобранный режим.
    pub op_mode: OpMode,
    /// Сырое значение `SAFETY_STS`.
    pub safety_sts: u8,
    /// Сырое значение `FAULT1_STS`.
    pub fault1_sts: u8,
    /// Сырое значение `FAULT2_STS`.
    pub fault2_sts: u8,
    /// Сырое значение `LDO_STS`.
    pub ldo_sts: u8,
}

impl Status {
    /// Активна ли петля ограничения входного тока.
    #[must_use]
    pub const fn iin_loop_active(&self) -> bool {
        self.sys_sts & regs::SYS_STS_IIN_LOOP != 0
    }

    /// Активна ли петля регулирования напряжения заряда.
    #[must_use]
    pub const fn vfloat_loop_active(&self) -> bool {
        self.sys_sts & regs::SYS_STS_VFLOAT_LOOP != 0
    }

    /// Заряд завершён.
    #[must_use]
    pub const fn charge_terminated(&self) -> bool {
        self.ldo_sts & regs::LDO_CHARGE_TERM != 0
    }

    /// Требуется дозаряд.
    #[must_use]
    pub const fn recharge_requested(&self) -> bool {
        self.ldo_sts & regs::LDO_RECHARGE != 0
    }

    /// Истёк сторожевой таймер.
    #[must_use]
    pub const fn watchdog_expired(&self) -> bool {
        self.fault1_sts & regs::FAULT1_WATCHDOG != 0
    }

    /// Есть ли критичный отказ, требующий вмешательства.
    #[must_use]
    pub const fn has_critical_fault(&self) -> bool {
        self.fault1_sts & (regs::FAULT1_WATCHDOG | regs::FAULT1_VBAT_OV | regs::FAULT1_VAC_OV) != 0
            || self.fault2_sts & regs::FAULT2_IIN_OC != 0
            || self.safety_sts & (regs::SAFETY_NTC_SHUTDOWN | regs::SAFETY_TEMP_MAX) != 0
    }

    /// Текстовое описание отказа для журнала (пустая строка, если отказов нет).
    ///
    /// Приоритет отдан аппаратным защитам: перегрев и уход батареи за порог
    /// опаснее, чем истёкший сторожевой таймер.
    #[must_use]
    pub const fn fault_summary(&self) -> &'static str {
        if self.safety_sts & regs::SAFETY_TEMP_MAX != 0 {
            "temp_max"
        } else if self.safety_sts & regs::SAFETY_NTC_SHUTDOWN != 0 {
            "ntc_shutdown"
        } else if self.fault1_sts & regs::FAULT1_VBAT_OV != 0 {
            "vbat_ov"
        } else if self.fault1_sts & regs::FAULT1_VAC_OV != 0 {
            "vac_ov"
        } else if self.fault2_sts & regs::FAULT2_IIN_OC != 0 {
            "iin_oc"
        } else if self.fault1_sts & regs::FAULT1_WATCHDOG != 0 {
            "watchdog"
        } else if self.fault1_sts & regs::FAULT1_VAC_UNPLUG != 0 {
            "vac_unplug"
        } else {
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_registers_follow_driver_mapping() {
        // Коды 10-битные и читаются парой от регистра канала; каналы
        // пересекаются по байтам — это свойство аппаратуры, а не ошибка.
        assert_eq!(AdcChannel::Iin.register(), regs::ADC_FIRST_STS);
        assert_eq!(AdcChannel::Vac.register(), regs::ADC_FIRST_STS + 1);
        assert_eq!(AdcChannel::Vin.register(), regs::ADC_FIRST_STS + 2);
        assert_eq!(AdcChannel::Vout.register(), regs::ADC_FIRST_STS + 3);
        // Регистр ADC05 (0x0D) не используется: VBAT и далее идут на один дальше.
        assert_eq!(AdcChannel::Vbat.register(), regs::ADC_FIRST_STS + 5);
        assert_eq!(AdcChannel::DieTemp.register(), regs::ADC_FIRST_STS + 6);
        assert_eq!(AdcChannel::TsBat.register(), regs::ADC_FIRST_STS + 7);
        assert_eq!(AdcChannel::TsBus.register(), regs::ADC_FIRST_STS + 8);
        assert_eq!(AdcChannel::Vbat.adc_index(), 6);
        assert_eq!(AdcChannel::TsBus.adc_index(), 9);
        assert_eq!(regs::ADC_LAST_STS, 0x12);
    }

    #[test]
    fn adc_decoding_matches_constants() {
        assert_eq!(AdcChannel::Vbat.decode(0), 1_000_000);
        assert_eq!(AdcChannel::Vbat.decode(688), 4_440_000);
        assert_eq!(AdcChannel::Iin.decode(100), 489_000);
        assert_eq!(AdcChannel::Vin.decode(100), 1_600_000);
        assert_eq!(AdcChannel::Vac.decode(0), 80_000);
        // Формула эталона: (935 - raw) * 4350 / 1000, ограничение [-250; 1600].
        assert_eq!(AdcChannel::DieTemp.decode(935), 0);
        assert_eq!(AdcChannel::DieTemp.decode(900), 152);
        assert_eq!(AdcChannel::DieTemp.decode(0), 1_600);
        assert_eq!(AdcChannel::DieTemp.decode(2_000), -250);
        assert_eq!(AdcChannel::TsBat.decode(1_000), 2_933_000);
    }

    #[test]
    fn status_flags_are_interpreted() {
        let status = Status {
            sys_sts: regs::SYS_STS_SWITCHING_ENABLED | regs::SYS_STS_IIN_LOOP,
            op_mode: OpMode::Switching,
            safety_sts: 0,
            fault1_sts: 0,
            fault2_sts: 0,
            ldo_sts: regs::LDO_CHARGE_TERM,
        };
        assert!(status.iin_loop_active());
        assert!(!status.vfloat_loop_active());
        assert!(status.charge_terminated());
        assert!(!status.has_critical_fault());
        assert_eq!(status.fault_summary(), "");
    }

    #[test]
    fn critical_faults_are_detected() {
        let watchdog = Status {
            sys_sts: 0,
            op_mode: OpMode::Standby,
            safety_sts: 0,
            fault1_sts: regs::FAULT1_WATCHDOG,
            fault2_sts: 0,
            ldo_sts: 0,
        };
        assert!(watchdog.has_critical_fault());
        assert!(watchdog.watchdog_expired());
        assert_eq!(watchdog.fault_summary(), "watchdog");

        let overtemp = Status {
            safety_sts: regs::SAFETY_TEMP_MAX,
            ..watchdog
        };
        assert!(overtemp.has_critical_fault());
        assert_eq!(overtemp.fault_summary(), "temp_max");
    }
}
