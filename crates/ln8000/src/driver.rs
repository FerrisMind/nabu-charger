//! Драйвер charge pump LN8000: проверка чипа, инициализация, режимы, статус.
//!
//! Логика повторяет `ln8000_init_device()` и `ln8000_change_opmode()` из
//! эталонного драйвера Android, чтобы под Windows устройство настраивалось так
//! же, как под Android.
//!
//! Драйвер не блокируется и не спит: всё, что требует времени (пауза после
//! сброса, обслуживание сторожевого таймера), — задача вызывающей стороны.
//!
//! # Пример
//!
//! ```
//! use ln8000::testkit::MockPumpBus;
//! use ln8000::{OpMode, Pump, PumpConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let bus = MockPumpBus::new();
//! let mut pump = Pump::open(bus, PumpConfig::default())?;
//! pump.configure()?;
//! assert_eq!(pump.enable_switching()?, OpMode::Switching);
//! let status = pump.status()?;
//! assert_eq!(status.op_mode, OpMode::Switching);
//! # Ok(())
//! # }
//! ```

use crate::encoding::{
    AdcHibernateDelay, AdcMode, NABU_VBAT_FLOAT_UV, OpMode, POR_VIN_TOLERANCE_UV,
    VBAT_TAPER_IIN_UA, WatchdogPeriod, bypass_allowed_by_vin, charge_mode, decode_iin_limit,
    encode_iin_limit, encode_ntc_alarm, encode_vac_ovp, encode_vbat_float, soft_float_for_vbat,
    vbat_near_float_with_vin,
};
use crate::error::{BusError, PumpError};
use crate::regs;
use crate::status::{AdcChannel, Status};
use crate::transport::RegisterBus;

/// Настройки драйвера.
///
/// Флаги защит намеренно повторяют имена из Device Tree: это независимые
/// аппаратные переключатели насоса, а не «булев суп» из логики приложения.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpConfig {
    /// Целевое напряжение заряда, мкВ (по умолчанию 4.44 В).
    pub vbat_float_uv: u32,
    /// Порог перенапряжения входа, мкВ (по умолчанию 9.5 В).
    pub vac_ovp_uv: u32,
    /// Лимит входного тока, мкА (по умолчанию 2 А).
    pub iin_limit_ua: u32,
    /// Порог аларма NTC (10 бит, по умолчанию 226 ≈ +40 °C).
    pub ntc_alarm_cfg: u16,
    /// Включать ли сторожевой таймер.
    pub watchdog_enabled: bool,
    /// Период сторожевого таймера.
    pub watchdog_period: WatchdogPeriod,
    /// Включать ли автовосстановление после отказов.
    pub auto_recovery: bool,
    /// Проверять каждую запись чтением.
    pub verify_writes: bool,
    /// Сколько раз повторять операцию при сбое шины.
    pub max_bus_retries: u8,
    // --- флаги защит (имена повторяют Device Tree планшета) ---
    //
    // В Android-схеме регулированием и температурой занимается главный зарядник
    // (SMB), а LN8000 работает каскадом 2:1, поэтому DTS *отключает* защиты
    // самого насоса. Мы не зашиваем выбор в код: какой вариант нужен под Windows
    // (где главный стек может не заряжать вообще), решается профилем.
    /// Отключить регуляцию напряжения заряда (`vbat-reg-disable`).
    pub vbat_reg_disabled: bool,
    /// Отключить защиту по току входа (`iin-ocp-disable`).
    pub iin_ocp_disabled: bool,
    /// Отключить регуляцию входного тока (`iin-reg-disable`).
    pub iin_reg_disabled: bool,
    /// Отключить защиту кристалла по температуре (`tdie-prot-disable`).
    pub tdie_prot_disabled: bool,
    /// Отключить регуляцию по температуре кристалла (`tdie-reg-disable`).
    pub tdie_reg_disabled: bool,
    /// Отключить мониторинг температуры шины (`tbus-mon-disable`).
    pub tbus_mon_disabled: bool,
    /// Отключить мониторинг температуры батареи (`tbat-mon-disable`).
    pub tbat_mon_disabled: bool,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            // Значения по умолчанию — из `ln8000_charger.h`
            // (`LN8000_BAT_OVP_DEFAULT`, `LN8000_BUS_OVP_DEFAULT`,
            // `LN8000_IIN_CFG_DEFAULT`, `LN8000_NTC_ALARM_CFG_DEFAULT`).
            // Android nabu: bat_ovp 4560 mV → V_FLOAT ≈ 4470 mV (ovp = float×1.02).
            vbat_float_uv: NABU_VBAT_FLOAT_UV,
            vac_ovp_uv: 9_500_000,
            iin_limit_ua: 2_000_000,
            ntc_alarm_cfg: regs::NTC_ALARM_DEFAULT,
            watchdog_enabled: false,
            watchdog_period: WatchdogPeriod::Sec10,
            auto_recovery: false,
            verify_writes: true,
            max_bus_retries: 2,
            vbat_reg_disabled: false,
            iin_ocp_disabled: false,
            iin_reg_disabled: false,
            tdie_prot_disabled: false,
            tdie_reg_disabled: false,
            tbus_mon_disabled: false,
            tbat_mon_disabled: false,
        }
    }
}

impl PumpConfig {
    /// Профиль по Device Tree планшета: защиты насоса отключены.
    ///
    /// Флаги взяты из `nabu-sm8150.dtsi` (`tdie-prot-disable`,
    /// `iin-ocp-disable`, `iin-reg-disable`, `tdie-reg-disable`,
    /// `vbat-reg-disable`, `tbus-mon-disable`, `tbat-mon-disable`).
    /// Именно этот набор Xiaomi считает правильным для nabu: регулирование
    /// держит главный зарядник, а насос работает каскадом.
    #[must_use]
    pub fn for_nabu_dts() -> Self {
        Self {
            vbat_reg_disabled: true,
            iin_ocp_disabled: true,
            iin_reg_disabled: true,
            tdie_prot_disabled: true,
            tdie_reg_disabled: true,
            tbus_mon_disabled: true,
            tbat_mon_disabled: true,
            ..Self::default()
        }
    }

    /// Применяет параметр из реестра к профилю.
    ///
    /// Имена совпадают с параметрами INF (`HKR, Parameters, ...`), поэтому тот,
    /// кто ставит драйвер, может менять пороги **без пересборки**.
    ///
    /// Возвращает `true`, если параметр известен и принят. `false` означает, что
    /// имя неизвестно или значение вне допустимых границ — профиль не меняется,
    /// то есть неверное значение не может тихо испортить настройки.
    ///
    /// Параметр `TelemetryMs` сюда не входит: период таймера — дело драйвера.
    #[must_use]
    pub fn apply_parameter(&mut self, name: &str, value: u32) -> bool {
        match name {
            // Границы входного тока проверяются тем же кодированием, что идёт
            // в чип: вне диапазона оно вернёт ошибку, и значение не принимается.
            "IinLimitUa" => {
                if encode_iin_limit(value).is_err() {
                    return false;
                }
                self.iin_limit_ua = value;
                true
            }
            // Границы из эталонного заголовка: `LN8000_VBAT_FLOAT_MIN/MAX`.
            "VbatFloatUv" => {
                if !(3_725_000..=5_000_000).contains(&value) {
                    return false;
                }
                self.vbat_float_uv = value;
                true
            }
            // `LN8000_VAC_OVP_6P5V` … `_13V`.
            "VacOvpUv" => {
                if !(6_500_000..=13_000_000).contains(&value) {
                    return false;
                }
                self.vac_ovp_uv = value;
                true
            }
            // Порог аларма NTC — 10 бит.
            "NtcAlarmCfg" => {
                if value > 0x03FF {
                    return false;
                }
                self.ntc_alarm_cfg = u16::try_from(value).unwrap_or(self.ntc_alarm_cfg);
                true
            }
            "WatchdogEnabled" => {
                self.watchdog_enabled = value != 0;
                true
            }
            // Сколько раз повторять операцию при сбое шины.
            "BusRetryCount" => {
                if value > 8 {
                    return false;
                }
                self.max_bus_retries = u8::try_from(value).unwrap_or(self.max_bus_retries);
                true
            }
            // 0 — как в Device Tree планшета (защиты насоса выключены),
            // 1 — с включёнными петлями. Выбор задаётся при установке.
            "ProtectionProfile" => {
                let template = match value {
                    0 => Self::for_nabu_dts(),
                    1 => Self::protective(),
                    _ => return false,
                };
                self.vbat_reg_disabled = template.vbat_reg_disabled;
                self.iin_ocp_disabled = template.iin_ocp_disabled;
                self.iin_reg_disabled = template.iin_reg_disabled;
                self.tdie_prot_disabled = template.tdie_prot_disabled;
                self.tdie_reg_disabled = template.tdie_reg_disabled;
                self.tbus_mon_disabled = template.tbus_mon_disabled;
                self.tbat_mon_disabled = template.tbat_mon_disabled;
                true
            }
            _ => false,
        }
    }

    /// Профиль с включёнными защитами насоса.
    ///
    /// Вариант для случая, когда главный стек под Windows батарею не заряжает и
    /// регулирование должно делать сам насос. Отличается от [`Self::default()`]
    /// только явным перечислением: удобно как альтернатива для прогона на железе.
    #[must_use]
    pub fn protective() -> Self {
        Self {
            vbat_reg_disabled: false,
            iin_ocp_disabled: false,
            iin_reg_disabled: false,
            tdie_prot_disabled: false,
            tdie_reg_disabled: false,
            tbus_mon_disabled: false,
            tbat_mon_disabled: false,
            ..Self::default()
        }
    }

    /// Профиль для работы через charge pump от блока Quick Charge 3.5 класса B.
    ///
    /// Пороги соответствуют `BUS_OVP_FOR_QC`, `BUS_OCP_FOR_QC3P5_CLASS_B`
    /// из `ln8000_charger.h`.
    ///
    /// Android DTS disables LN8000 VFLOAT/IIN loops because SMB does CV. Under
    /// Windows the PEIC path owns charging — keep VFLOAT + IIN regulation on so
    /// VBAT cannot idle at `Vin/2` (~4.78 V) and starve Iin.
    #[must_use]
    pub fn for_qc35_class_b() -> Self {
        Self {
            vac_ovp_uv: 13_000_000,
            iin_limit_ua: 3_500_000 - 700_000,
            vbat_reg_disabled: false,
            iin_reg_disabled: false,
            // Thermal / OCP monitors still follow nabu DTS (SMB-era defaults).
            iin_ocp_disabled: true,
            tdie_prot_disabled: true,
            tdie_reg_disabled: true,
            tbus_mon_disabled: true,
            tbat_mon_disabled: true,
            ..Self::default()
        }
    }

    /// Профиль для проверки режима 2:1 без высокого напряжения.
    #[must_use]
    pub fn conservative() -> Self {
        Self {
            vac_ovp_uv: 6_500_000,
            iin_limit_ua: 1_000_000,
            ..Self::default()
        }
    }
}

/// Состояние сессии драйвера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpState {
    /// Сессия не открыта.
    Closed,
    /// Чип опознан.
    Probed,
    /// Пороги и защиты настроены.
    Configured,
    /// Режим 2:1 включён.
    Switching,
    /// Устройство в отказе.
    Faulted,
}

impl PumpState {
    /// Имя состояния для журнала.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Probed => "probed",
            Self::Configured => "configured",
            Self::Switching => "switching",
            Self::Faulted => "faulted",
        }
    }
}

/// Драйвер charge pump поверх абстрактной шины I²C.
#[derive(Debug)]
pub struct Pump<T: RegisterBus> {
    bus: T,
    config: PumpConfig,
    state: PumpState,
    op_mode: OpMode,
    /// Стадия, которой достиг 5-вольтовый резерв в последней попытке: `0` — не
    /// запрашивался, `1` — чистая запись режима, `2` — POR с профилем, `3` —
    /// маска отказов. См. [`Self::bypass_stage`].
    bypass_stage: u8,
    /// Вход, на котором уже снимали защёлку через POR.
    ///
    /// Эталон ставит POR **один раз** и после него чип слушается; повторять
    /// сброс на каждой попытке нельзя — это дёргает заряд и стирает состояние,
    /// которое чип мог защёлкнуть законно. Пока Vin не сменился больше чем на
    /// [`POR_VIN_TOLERANCE_UV`], второй POR не делается.
    por_vin_uv: Option<u32>,
    /// Сколько записей и чтений выполнено (для отчётов).
    writes: u32,
    reads: u32,
}

impl<T: RegisterBus> Pump<T> {
    /// Открывает сессию: проверяет связь и идентификатор устройства.
    ///
    /// # Errors
    ///
    /// * [`PumpError::Bus`] — шина недоступна.
    /// * [`PumpError::WrongDeviceId`] — ответ не равен [`regs::DEVICE_ID_VALUE`].
    pub fn open(mut bus: T, config: PumpConfig) -> Result<Self, PumpError> {
        let _ = bus.reset();
        let id = bus.read(regs::DEVICE_ID)?;
        if id != regs::DEVICE_ID_VALUE {
            return Err(PumpError::WrongDeviceId { got: id });
        }
        Ok(Self {
            bus,
            config,
            state: PumpState::Probed,
            op_mode: OpMode::Unknown,
            bypass_stage: 0,
            por_vin_uv: None,
            writes: 0,
            reads: 1,
        })
    }

    /// Текущее состояние сессии.
    #[must_use]
    pub const fn state(&self) -> PumpState {
        self.state
    }

    /// Последний известный режим.
    #[must_use]
    pub const fn op_mode(&self) -> OpMode {
        self.op_mode
    }

    /// Стадия 5-вольтового резерва, которой достигла последняя попытка.
    ///
    /// `0` — режим 1:1 не запрашивался; `1` — чистая запись `SYS_CTRL` без
    /// правок `FAULT_CTRL` (именно она работала 17.09); `2` — POR (`soft_reset`,
    /// пауза, `configure`, запись режима); `3` — надстройка `.627` с маской
    /// UV/OV и импульсом снятия защёлки. Вендор ни стадию 2, ни стадию 3 не
    /// документирует: 2 подтверждена живыми прогонами, 3 — нет.
    #[must_use]
    pub const fn bypass_stage(&self) -> u8 {
        self.bypass_stage
    }

    /// Потрачен ли POR-бюджет текущего входа: `true`, если защёлку на этом Vin
    /// уже пытались снять. По этой марке видно, работает драйвер на первом
    /// заходе или уже упёрся в отказ и ждёт смены блока питания.
    #[must_use]
    pub const fn por_spent(&self) -> bool {
        self.por_vin_uv.is_some()
    }

    /// Доступ к шине только для чтения: диагностика и тесты.
    #[must_use]
    pub const fn bus(&self) -> &T {
        &self.bus
    }

    /// Изменяемый доступ к шине: подготовка состояний в диагностике и тестах.
    pub fn bus_mut(&mut self) -> &mut T {
        &mut self.bus
    }

    /// Имя шины.
    #[must_use]
    pub fn bus_name(&self) -> &'static str {
        self.bus.name()
    }

    /// Сколько записей и чтений выполнено.
    #[must_use]
    pub const fn counters(&self) -> (u32, u32) {
        (self.writes, self.reads)
    }

    /// Настройки сессии.
    #[must_use]
    pub const fn config(&self) -> &PumpConfig {
        &self.config
    }

    /// Настраивает пороги и защиты.
    ///
    /// Порядок действий повторяет `ln8000_init_device()`:
    ///
    /// 1. напряжение заряда (`V_FLOAT_CTRL`);
    /// 2. порог перенапряжения входа (`GLITCH_CTRL[3:2]`);
    /// 3. лимит входного тока (`IIN_CTRL[6:0]`);
    /// 4. порог NTC (`NTC_CTRL` + `ADC_CTRL[1:0]`);
    /// 5. конфигурация защиты NTC (`REGULATION_CTRL[3:2]`);
    /// 6. автовосстановление (`RECOVERY_CTRL[7:4]`);
    /// 7. включение защит и петель регулирования;
    /// 8. перевод в standby;
    /// 9. сторожевой таймер, АЦП и мониторы температур;
    /// 10. отметка о программной инициализации и пороги.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::Bus`], [`PumpError::OutOfRange`] — сбой шины или значения.
    pub fn configure(&mut self) -> Result<(), PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }

        // 1. напряжение заряда
        let vfloat = encode_vbat_float(self.config.vbat_float_uv);
        self.write_verified(regs::V_FLOAT_CTRL, vfloat, "vbat_float")?;

        // 2. порог перенапряжения входа
        let vac = encode_vac_ovp(self.config.vac_ovp_uv);
        self.update(regs::GLITCH_CTRL, 0x03 << 2, vac << 2, "vac_ovp")?;

        // 3. лимит входного тока (как в драйвере: лимит = OCP − 700 мА)
        let iin_code = encode_iin_limit(self.config.iin_limit_ua)?;
        self.update(regs::IIN_CTRL, 0x7F, iin_code, "iin_limit")?;

        // 4. порог NTC: младшие биты в NTC_CTRL, старшие — в ADC_CTRL
        let (low, high) = encode_ntc_alarm(self.config.ntc_alarm_cfg);
        self.write_verified(regs::NTC_CTRL, low, "ntc_alarm_low")?;
        self.update(regs::ADC_CTRL, 0x03, high, "ntc_alarm_high")?;

        // 5. конфигурация защиты NTC по температуре
        self.update(
            regs::REGULATION_CTRL,
            0x03 << 2,
            regs::NTC_SHUTDOWN_CFG << 2,
            "ntc_shutdown_cfg",
        )?;

        // 6. автовосстановление и мониторы температур шины и батареи (биты 1:0).
        let recovery = if self.config.auto_recovery {
            0xF0
        } else {
            0x00
        };
        let monitors = u8::from(!self.config.tbus_mon_disabled) << 1
            | u8::from(!self.config.tbat_mon_disabled);
        self.update(
            regs::RECOVERY_CTRL,
            0xF0 | 0b11,
            recovery | monitors,
            "recovery_and_monitors",
        )?;

        // 7. защиты и петли регулирования — по флагам профиля.
        self.configure_protections()?;

        // 8. перевести в standby
        self.set_op_mode(OpMode::Standby)?;
        self.update(regs::FAULT_CTRL, 1 << 4, 0, "enable_vac_ov")?;

        // 9. сторожевой таймер и АЦП
        let wdt = if self.config.watchdog_enabled {
            1u8 << 7
        } else {
            0
        };
        self.update(regs::TIMER_CTRL, 1 << 7, wdt, "watchdog_enable")?;
        self.update(
            regs::TIMER_CTRL,
            0x03 << 5,
            self.config.watchdog_period.code() << 5,
            "watchdog_period",
        )?;
        self.update(
            regs::ADC_CTRL,
            0x07 << 5,
            AdcMode::Shutdown.code() << 5,
            "adc_off_before_config",
        )?;
        self.update(
            regs::ADC_CTRL,
            0x03 << 3,
            AdcHibernateDelay::Sec4.code() << 3,
            "adc_hibernate_delay",
        )?;
        // Все каналы АЦП (как `ln8000_set_adc_ch(ALL, true)`).
        self.write_verified(regs::ADC_CFG, 0x3E, "adc_channels")?;
        self.update(
            regs::ADC_CTRL,
            0x07 << 5,
            AdcMode::AutoHibernate.code() << 5,
            "adc_auto",
        )?;

        // 10. отметка инициализации и пороги
        self.update(regs::CHARGE_CTRL, 1 << 7, 1 << 7, "sw_init_marker")?;
        self.write_verified(
            regs::THRESHOLD_CTRL,
            regs::THRESHOLD_CTRL_DEFAULT,
            "thresholds",
        )?;

        self.state = PumpState::Configured;
        Ok(())
    }

    /// Записывает биты защит и петель регулирования по флагам профиля.
    ///
    /// В Device Tree планшета часть защит самого насоса отключена
    /// (`tdie-prot-disable`, `iin-ocp-disable`, `iin-reg-disable`,
    /// `tdie-reg-disable`, `vbat-reg-disable`): в Android-схеме регулированием
    /// занимается главный зарядник SMB, а насос работает каскадом 2:1. Здесь это
    /// следует за профилем, а не зашито в код.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::Bus`] — сбой шины.
    fn configure_protections(&mut self) -> Result<(), PumpError> {
        let cfg = self.config;
        self.update(regs::FAULT_CTRL, 1 << 5, 0, "enable_vbat_ovp")?;
        // `FAULT_CTRL` IIN_OCP выключается **всегда**, независимо от профиля:
        // вендорский DT планшета её не оставляет (`ln8000_charger,
        // iin-ocp-disable`, `nabu-sm8150.dtsi:239`), а `bus-ocp-threshold = 3750`
        // мА задан там только как аларм. Включённая защита защёлкивает
        // `FAULT2_IIN_OC` на первом же включении 2:1 — живой замер 19.09 11:10:
        // шина 8,256 В, `PostHvdcpMode = 3`, следом `FAULT2 = 0x80`,
        // `SuMode = 1`, 39,1 мА, `EngageState = 0` — заряд не идёт вовсе.
        // Профиль по-прежнему управляет петлями (`iIN_REG`/`VFLOAT`), но не
        // этой защёлкой.
        self.update(regs::FAULT_CTRL, 1 << 6, 1 << 6, "iin_ocp_off_nabu_dts")?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 5,
            u8::from(cfg.vbat_reg_disabled) << 5,
            "vfloat_loop",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 7,
            u8::from(!cfg.vbat_reg_disabled) << 7,
            "vfloat_loop_int",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 4,
            u8::from(cfg.iin_reg_disabled) << 4,
            "iin_loop",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 6,
            u8::from(!cfg.iin_reg_disabled) << 6,
            "iin_loop_int",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 2,
            u8::from(!cfg.tdie_prot_disabled) << 2,
            "tdie_prot",
        )?;
        self.update(
            regs::REGULATION_CTRL,
            1 << 1,
            u8::from(!cfg.tdie_reg_disabled) << 1,
            "tdie_regulation",
        )?;
        self.update(regs::SYS_CTRL, 1 << 2, 0, "disable_reverse_current")
    }

    /// Включает режим 2:1 (switching) и проверяет, что устройство его приняло.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::ModeNotReached`] — `SYS_STS` не подтвердил режим.
    pub fn enable_switching(&mut self) -> Result<OpMode, PumpError> {
        self.set_op_mode(OpMode::Switching)?;
        let status = self.settle_and_read_status()?;
        if status.op_mode != OpMode::Switching {
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Switching.code(),
                raw_status: status.sys_sts,
            });
        }
        self.state = PumpState::Switching;
        Ok(status.op_mode)
    }

    /// Включает режим 1:1 (bypass) — например, для зарядки от 5 В.
    ///
    /// Сам режим допустим только в окне обхода: `EN_1TO1` подаёт вход прямо на
    /// батарею, поэтому при `Vin >= 8 В` (и ниже 4,2 В) вызов отклоняется с
    /// [`PumpError::BypassNeedsFiveVoltVin`] — проверка внутри, чтобы ни один
    /// вызывающий не мог включить 1:1 на повышенном входе.
    ///
    /// # Errors
    ///
    /// * [`PumpError::BypassNeedsFiveVoltVin`] — Vin вне окна обхода.
    /// * [`PumpError::ModeNotReached`] — чип не подтвердил режим.
    /// * [`PumpError::Bus`] / [`PumpError::NotOpen`] — шина или сессия.
    pub fn enable_bypass(&mut self) -> Result<OpMode, PumpError> {
        let vin = self.read_adc(AdcChannel::Vin)?;
        let vbat = u32::try_from(self.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
        if !bypass_allowed_by_vin(vin, vbat) {
            return Err(PumpError::BypassNeedsFiveVoltVin { vin_uv: vin });
        }
        self.set_op_mode(OpMode::Bypass)?;
        let status = self.settle_and_read_status()?;
        // Проверяем так же, как для режима 2:1: молчаливый отказ чипа нельзя
        // принимать за успех — иначе драйвер решит, что резервный режим включён,
        // когда на самом деле заряд не идёт.
        if status.op_mode != OpMode::Bypass {
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Bypass.code(),
                raw_status: status.sys_sts,
            });
        }
        // 1:1 — это не 2:1: состояние сессии должно называться честно.
        self.state = PumpState::Configured;
        Ok(status.op_mode)
    }

    /// Включает режим 2:1, а при неудаче — безопасный bypass.
    ///
    /// Возвращает фактически достигнутый режим: `Switching`, если чип подтвердил
    /// ускоренный режим, или `Bypass`, если пришлось отступить. Так драйвер
    /// не остаётся без рабочего режима из-за одного отказа чипа или шины.
    ///
    /// Если не подтверждается ни то, ни другое — возвращается ошибка первой
    /// попытки (она говорит именно про режим); тогда вызывающий обязан увести чип
    /// в `standby`, иначе он останется в неопределённом состоянии.
    ///
    /// # Errors
    ///
    /// * [`PumpError::ModeNotReached`] — чип не подтвердил ни 2:1, ни bypass.
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::Bus`] — сбой шины.
    pub fn enable_switching_or_bypass(&mut self) -> Result<OpMode, PumpError> {
        match self.enable_switching() {
            Ok(mode) => Ok(mode),
            Err(switching_error) => match self.enable_bypass() {
                Ok(mode) => Ok(mode),
                Err(_) => Err(switching_error),
            },
        }
    }

    /// Перечитывает состояние, давая чипу время применить режим.
    ///
    /// Эталон после записи режима ждёт 10 мс (`msleep(10)`) и только затем
    /// читает `SYS_STS`. Пустой цикл ожидания в ядре нам недоступен, поэтому
    /// перечитываем состояние несколько раз: один обмен по шине занимает
    /// 8-46 мс, что заведомо больше паузы эталона.
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибки шины.
    fn settle_and_read_status(&mut self) -> Result<Status, PumpError> {
        let mut last = self.status()?;
        for _ in 0..3 {
            last = self.status()?;
            if last.op_mode != OpMode::Unknown && last.op_mode != OpMode::Standby {
                break;
            }
        }
        Ok(last)
    }

    /// Explicit charge start/stop, mirroring Android `psy_chg_set_charging_enable`
    /// plus Vin/Vbat-aware mode selection (`cp_qc30` style).
    ///
    /// Order: disable RCP → near-float soft OV/float/taper → clear latched faults
    /// → pick mode from Vin **and Vbat** → request mode → settle/read back.
    /// 2:1 needs `Vin >= 2*Vbat + 250 mV` (and `>= 8 V`); it is never replaced by
    /// 1:1 bypass at elevated Vin — that would put 8 V+ across the battery.
    /// When Vin is elevated but short of `2*Vbat + 250 mV`, no mode is requested
    /// (standby; the caller retries under its own cooldown, not every tick).
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] - помпа не открыта.
    /// * [`PumpError::Bus`] - сбой обмена.
    /// * [`PumpError::ModeNotReached`] - chip refused the Vin-appropriate mode.
    ///
    /// `post_reset_delay` обязателен: 5-вольтовый путь восстановления делает
    /// `soft_reset`, после которого POR запрещает любой обмен по I²C до
    /// [`regs::SOFT_RESET_DELAY_MS`]. Хост-тесты передают пустое замыкание,
    /// KMDF — `KeDelayExecutionThread`; забыть задержку нельзя, потому что без
    /// параметра функция не вызывается.
    pub fn set_charging(
        &mut self,
        on: bool,
        post_reset_delay: &mut dyn FnMut(),
    ) -> Result<OpMode, PumpError> {
        // Шаг эталона: перед стартом заряда обратная защита выключается.
        self.update(regs::SYS_CTRL, 1 << 2, 0, "disable_reverse_current")?;
        if !on {
            // Заряд выключен — узел считается разомкнутым: POR-бюджет входа
            // сбрасывается, чтобы следующее подключение имело право на сброс.
            self.por_vin_uv = None;
            self.set_op_mode(OpMode::Standby)?;
            let status = self.settle_and_read_status()?;
            self.op_mode = status.op_mode;
            self.state = PumpState::Configured;
            return Ok(status.op_mode);
        }

        let vin = self.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vbat = u32::try_from(self.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
        let Some(want) = charge_mode(vin, vbat) else {
            let _ = self.set_op_mode(OpMode::Standby);
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Standby.code(),
                raw_status: 0,
            });
        };

        // Watchdog status bit also blocks mode until cleared; force WDT off when
        // the profile disabled it (registry WatchdogEnabled=0).
        if !self.config.watchdog_enabled {
            let _ = self.update(regs::TIMER_CTRL, 1 << 7, 0, "watchdog_force_off");
        }

        self.bypass_stage = 0;

        // Near-float / FAULT1_VBAT_OV: Android cp_qc30 tapers and hands off to
        // SMB; on Windows we still need mode 3. Soft-raise V_FLOAT slightly,
        // mask VBAT_OV (like VIN_OV for QC), clear latch, taper IIN. Never use
        // 1:1 bypass at elevated Vin even if the battery is near full.
        //
        // Для 5-вольтового резерва ни тапер, ни снятие защёлки до запроса
        // режима не делаются: проверенная 17.09 последовательность состояла из
        // снятия обратной защиты (`SYS_CTRL` бит 2) и записи режима. Всё
        // остальное — стадии 2–3 (`recover_5v_bypass`), и они применяются
        // только после того, как чип отказал на чистом пути.
        if want != OpMode::Bypass {
            self.prepare_near_float_for_charge(true);

            // Live nabu: FAULT1 VIN_OV latches at QC ~12 V and blocks mode change
            // (volt_qual). Clear latch; mask VIN_OV for the elevated 2:1 path only.
            let _ = self.clear_latched_faults();
            if want == OpMode::Switching {
                let _ = self.update(
                    regs::FAULT_CTRL,
                    regs::FAULT_CTRL_DISABLE_VIN_OV,
                    regs::FAULT_CTRL_DISABLE_VIN_OV,
                    "disable_vin_ov_qc",
                );
            }
        }

        let result = match want {
            OpMode::Switching => self.enable_switching(),
            OpMode::Bypass => {
                self.bypass_stage = 1;
                self.enable_bypass()
                    .or_else(|_| self.recover_5v_bypass(post_reset_delay))
            }
            OpMode::Standby | OpMode::Unknown => Err(PumpError::ModeNotReached {
                wanted: want.code(),
                raw_status: 0,
            }),
        };

        match result {
            Ok(mode) => {
                self.op_mode = mode;
                self.state = match mode {
                    OpMode::Switching => PumpState::Switching,
                    _ => PumpState::Configured,
                };
                // Тапер и смягчение `VBAT_OV` — после подтверждённого режима:
                // на запрос режима они уже не влияют, а батарею у верха заряда
                // защищают. Уставку `V_FLOAT` сквозной режим не поднимает.
                if mode == OpMode::Bypass {
                    self.prepare_near_float_for_charge(false);
                }
                Ok(mode)
            }
            Err(err) => {
                // Do not force standby after a partial 5 V bypass arm — that
                // undoes SYS_CTRL=0x01 before the chip settles (live TA200).
                if want != OpMode::Bypass {
                    let _ = self.set_op_mode(OpMode::Standby);
                }
                Err(err)
            }
        }
    }

    /// Mask UV/OV that latch `FAULT1=0x21` on saggy 5 V bricks (TA200).
    fn arm_5v_bypass_fault_mask(&mut self) -> Result<(), PumpError> {
        self.update(
            regs::FAULT_CTRL,
            regs::FAULT_CTRL_MASK_5V_BYPASS,
            regs::FAULT_CTRL_MASK_5V_BYPASS,
            "mask_5v_bypass_faults",
        )?;
        self.clear_latched_faults()
    }

    /// Soft-reset recovery for the 5 V bypass, stage 2, then the masked
    /// stage 3 if the chip still refuses.
    ///
    /// Stage 2 is the sequence that worked on 17.09 both from the raw tool and
    /// from the driver: `soft_reset` → **caller delay** → `configure` (profile
    /// only) → `SYS_CTRL=0x01`. Stage 3 adds the `.627` overlay (mask UV/OV,
    /// pulse the latch) and is the last resort — nothing in the vendor sources
    /// asks for it. Never used at elevated Vin (would drop the QC latch).
    ///
    /// `post_reset_delay` must sleep ≥ [`regs::SOFT_RESET_DELAY_MS`] before any
    /// further I²C (POR). It comes from the caller of [`Self::set_charging`]:
    /// host tests pass a no-op, KMDF sleeps (`KeDelayExecutionThread`); without a
    /// real delay `configure()` right after `soft_reset` hangs the chip.
    fn recover_5v_bypass(
        &mut self,
        post_reset_delay: &mut dyn FnMut(),
    ) -> Result<OpMode, PumpError> {
        let vin_now =
            u32::try_from(self.read_adc(AdcChannel::Vin).unwrap_or(0).max(0)).unwrap_or(0);
        // POR — один раз на вход. Эталон снимает защёлку сбросом и после этого
        // чип слушается; повторять сброс каждый тик нельзя: это дёргает заряд и
        // стирает состояние, которое чип мог защёлкнуть законно.
        if let Some(prev) = self.por_vin_uv {
            // Исключение из бюджета: живая защёлка VFAULT (`FAULT1 = 0x21`).
            // Импульс `TIMER_CTRL` её не снимает — он чистит только FAULT2
            // (живой замер 0x3F → 0x20, FAULT1 не тронут), а с ней чип отказывает
            // в 1:1 на 4,7–5,0 В: замер 19.09 10:49 на MDY-11-EP — 39,1 мА,
            // mode 1, `ChargeAttemptN` растёт, `LastEnableErr = -4`. POR —
            // единственная живая последовательность, после которой `FAULT1=0x00`
            // и обход держит 2,0–2,7 А. Без защёлки бюджет действует как раньше.
            let fault1 = self.read(regs::FAULT1_STS).unwrap_or(0);
            if vin_now.abs_diff(prev) <= POR_VIN_TOLERANCE_UV
                && fault1 & regs::FAULT1_VFAULTS_MASK == 0
            {
                return Err(PumpError::ModeNotReached {
                    wanted: OpMode::Bypass.code(),
                    raw_status: self.read(regs::SYS_STS).unwrap_or(0),
                });
            }
        }
        self.por_vin_uv = Some(vin_now);

        let _ = self.soft_reset();
        self.bypass_stage = 2;
        post_reset_delay();
        let _ = self.configure();
        let vin = self.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vbat = u32::try_from(self.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
        if charge_mode(vin, vbat) != Some(OpMode::Bypass) {
            return Err(PumpError::ModeNotReached {
                wanted: OpMode::Bypass.code(),
                raw_status: 0,
            });
        }
        // Маскированная запись, как `ln8000_change_opmode` вендора (маска
        // `STANDBY_EN|EN_1TO1` = 0x09, `.c:697`): абсолютный `SYS_CTRL=0x01`
        // обнулял биты 7:4 и 1, которых вендор не трогает. На живой плате это
        // давало `SYS_STS=0x28` вместо `BYPASS_ENABLED`.
        self.set_op_mode(OpMode::Bypass)?;
        let status = self.settle_and_read_status()?;
        if status.op_mode == OpMode::Bypass {
            self.state = PumpState::Configured;
            self.prepare_near_float_for_charge(false);
            return Ok(status.op_mode);
        }

        // Стадия 3: маска UV/OV и импульс снятия защёлки. Надстройка `.627`,
        // вендором не документирована; на живой плате не проверена.
        self.bypass_stage = 3;
        let _ = self.arm_5v_bypass_fault_mask();
        self.set_op_mode(OpMode::Bypass)?;
        let status = self.settle_and_read_status()?;
        if status.op_mode == OpMode::Bypass {
            self.state = PumpState::Configured;
            return Ok(status.op_mode);
        }
        // Маска не помогла — вернуть `FAULT_CTRL` как было: оставлять узел с
        // выключенными защитами хуже, чем отказ режима.
        let _ = self.update(
            regs::FAULT_CTRL,
            regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "unmask_after_failed_bypass",
        );
        Err(PumpError::ModeNotReached {
            wanted: OpMode::Bypass.code(),
            raw_status: status.sys_sts,
        })
    }

    /// Намеренная уставка тапера у верха заряда, мкА.
    ///
    /// `Some(min(config.iin_limit_ua, VBAT_TAPER_IIN_UA))`, пока Vbat в полосе
    /// тапера, иначе `None`. Условие то же, что у
    /// `prepare_near_float_for_charge`: защёлкнутый `VBAT_OV` и отсев
    /// артефакта `VBAT ≈ Vin/2` — здесь оно определено **один раз**, чтобы тапер
    /// и защита не разошлись в трактовке.
    ///
    /// Нужна защите: её возврат к профилю (`guard::evaluate`) обязан
    /// останавливаться на этой уставке, иначе он отменяет намеренное снижение
    /// тока в окне, где полосы тапера и возврата пересекаются.
    #[must_use]
    pub fn taper_setpoint_ua(&self, vbat_uv: u32, vin_uv: u32, ov_latched: bool) -> Option<u32> {
        if !vbat_near_float_with_vin(vbat_uv, self.config.vbat_float_uv, ov_latched, vin_uv) {
            return None;
        }
        Some(self.config.iin_limit_ua.min(VBAT_TAPER_IIN_UA))
    }

    /// Soft float / `VBAT_OV` / taper when Vbat is near the Nabu float band.
    ///
    /// Matches live recovery (`V_FLOAT` ~4.50 V + latch clear) and Android
    /// taper-at-`bat_volt_lmt−100`. Does not permanently shrink the profile
    /// `iin_limit_ua` — only writes `IIN_CTRL` for this attempt.
    ///
    /// `allow_float_raise` разделяет два случая. Для 2:1 подъём `V_FLOAT`
    /// нужен: без него защёлкнутый `VBAT_OV` блокирует смену режима. Для 1:1
    /// он запрещён: проверенный прогон 17.09 шёл с `V_FLOAT = 0x7D` (4,35 В),
    /// то есть **ниже** профиля, и повышать уставку на сквозном режиме значило
    /// бы кормить батарею от 5 В до большего напряжения. Тапер тока от этого не
    /// зависит и работает в обоих случаях.
    fn prepare_near_float_for_charge(&mut self, allow_float_raise: bool) {
        let vbat = self.read_adc(AdcChannel::Vbat).unwrap_or(0);
        let vbat_uv = u32::try_from(vbat.max(0)).unwrap_or(0);
        let vin = self.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vin_uv = u32::try_from(vin.max(0)).unwrap_or(0);
        let fault1 = self.read(regs::FAULT1_STS).unwrap_or(0);
        let ov_latched = fault1 & regs::FAULT1_VBAT_OV != 0;
        let float_uv = self.config.vbat_float_uv;
        // Reject Vin/2 rail artifact (live: 4780 mV @ Vin 9.6 V while pack ~4.47 V).
        let Some(tapered) = self.taper_setpoint_ua(vbat_uv, vin_uv, ov_latched) else {
            return;
        };

        // Soft OV mask + float headroom only when OV is latched or Vbat is at
        // the float ceiling (not merely in the early taper band).
        let at_ceiling = ov_latched || vbat_uv.saturating_add(50_000) >= float_uv;
        if at_ceiling && allow_float_raise {
            let want_float = soft_float_for_vbat(float_uv, vbat_uv);
            if want_float > float_uv {
                // Write float without permanently raising the configured profile
                // beyond the soft max — keep session headroom for retries.
                let code = encode_vbat_float(want_float);
                let _ = self.write_verified(regs::V_FLOAT_CTRL, code, "vbat_float_soft");
            }
            let _ = self.update(
                regs::FAULT_CTRL,
                regs::FAULT_CTRL_DISABLE_VBAT_OV,
                regs::FAULT_CTRL_DISABLE_VBAT_OV,
                "disable_vbat_ov_near_float",
            );
            let _ = self.clear_latched_faults();
        }

        if tapered < self.config.iin_limit_ua {
            // Тапер идёт тем же путём записи, что и `set_iin_limit`, но профиль
            // (`config.iin_limit_ua`) не трогает: это уставка «на один заход»,
            // и `configure()` обязан вернуть профильный лимит. Кто именно стоит
            // в регистре, читает [`Self::applied_iin_ua`] — по нему защита решает
            // срез, иначе «снижение» подняло бы ток с 1,2 А обратно к 2 А.
            // Потолок возврата защита берёт из [`Self::taper_setpoint_ua`].
            let _ = self.write_iin_limit(tapered, "iin_taper_near_float");
        }
    }

    /// Pulse `TIMER_CTRL` bit 2 to clear latched fault/status (Android).
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] / [`PumpError::Bus`]
    pub fn clear_latched_faults(&mut self) -> Result<(), PumpError> {
        self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_CLEAR_LATCH,
            regs::TIMER_CTRL_CLEAR_LATCH,
            "latch_clear_set",
        )?;
        self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_CLEAR_LATCH,
            0,
            "latch_clear_clr",
        )
    }

    /// Переводит устройство в standby.
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибки шины.
    pub fn standby(&mut self) -> Result<(), PumpError> {
        self.set_op_mode(OpMode::Standby)
    }

    /// Обслуживает («кормит») сторожевой таймер чипа.
    ///
    /// Сторож — это защита, а не помеха: если драйвер перестанет отвечать, чип
    /// сам прекратит заряд через выбранный период (5/10/20/40 с). Поэтому тот,
    /// кто включил сторож через `PumpConfig::watchdog_enabled`, обязан вызывать
    /// эту функцию чаще периода — драйвер делает это из таймера телеметрии.
    ///
    /// Периодные биты не затрагиваются: перезапись идёт по маске одного бита 7.
    /// Если сторож выключен, вызов безопасен и не включает его.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — чип не открыт.
    /// * [`PumpError::Bus`] — сбой шины.
    pub fn service_watchdog(&mut self) -> Result<(), PumpError> {
        if !self.config.watchdog_enabled {
            return Ok(());
        }
        self.update(regs::TIMER_CTRL, 1 << 7, 1 << 7, "watchdog_service")
    }

    /// Обновляет лимит входного тока.
    ///
    /// # Errors
    ///
    /// * [`PumpError::OutOfRange`] — ток меньше [`regs::IIN_MIN_UA`].
    /// * [`PumpError::Bus`] — сбой шины.
    pub fn set_iin_limit(&mut self, iin_ua: u32) -> Result<u8, PumpError> {
        let code = self.write_iin_limit(iin_ua, "iin_limit")?;
        self.config.iin_limit_ua = iin_ua;
        Ok(code)
    }

    /// Записывает уставку входного тока в `IIN_CTRL`, не трогая профиль.
    ///
    /// Единая точка записи: так уставку ставят и [`Self::set_iin_limit`], и тапер
    /// у верха заряда. Прочитанный обратно регистр — источник истины о том, что
    /// реально стоит в чипе (см. [`Self::applied_iin_ua`]).
    ///
    /// # Errors
    ///
    /// * [`PumpError::OutOfRange`] — ток меньше [`regs::IIN_MIN_UA`].
    /// * [`PumpError::Bus`] — сбой шины.
    fn write_iin_limit(&mut self, iin_ua: u32, field: &'static str) -> Result<u8, PumpError> {
        let code = encode_iin_limit(iin_ua)?;
        self.update(regs::IIN_CTRL, 0x7F, code, field)?;
        Ok(code)
    }

    /// Фактически записанная уставка входного тока, мкА.
    ///
    /// Читается из регистра, а не из профиля: `config.iin_limit_ua` — это
    /// «сколько заказано», и тапер у верха заряда пишет мимо него (1,2 А при
    /// профиле 2,8 А). Решение о срезе обязано опираться на то, что стоит в чипе,
    /// иначе «снизить до 2 А» поднимет ток с 1,2 А.
    ///
    /// `None` — регистр не прочитан: уставка неизвестна, и решать по току нельзя.
    #[must_use]
    pub fn applied_iin_ua(&mut self) -> Option<u32> {
        self.read_register(regs::IIN_CTRL)
            .ok()
            .map(decode_iin_limit)
    }

    /// Обновляет целевое напряжение заряда.
    ///
    /// # Errors
    ///
    /// [`PumpError::Bus`] — сбой шины.
    pub fn set_vbat_float(&mut self, vbat_uv: u32) -> Result<u8, PumpError> {
        let code = encode_vbat_float(vbat_uv);
        self.write_verified(regs::V_FLOAT_CTRL, code, "vbat_float")?;
        self.config.vbat_float_uv = vbat_uv;
        Ok(code)
    }

    /// Читает снимок состояния.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::Bus`] — сбой шины.
    pub fn status(&mut self) -> Result<Status, PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        let sys_sts = self.read(regs::SYS_STS)?;
        let safety_sts = self.read(regs::SAFETY_STS)?;
        let fault1_sts = self.read(regs::FAULT1_STS)?;
        let fault2_sts = self.read(regs::FAULT2_STS)?;
        let ldo_sts = self.read(regs::LDO_STS)?;
        let op_mode = OpMode::from_sys_sts(sys_sts);
        self.op_mode = op_mode;
        Ok(Status {
            sys_sts,
            op_mode,
            safety_sts,
            fault1_sts,
            fault2_sts,
            ldo_sts,
        })
    }

    /// Читает показание одного канала АЦП.
    ///
    /// Код занимает два соседних регистра (10 бит), поэтому читается парой.
    ///
    /// # Errors
    ///
    /// [`PumpError::Bus`] — сбой шины.
    /// Читает значение канала АЦП.
    ///
    /// На время чтения двух байт отсчёта обновление АЦП останавливается, а затем
    /// возобновляется — иначе байты могут прийти из разных преобразований, и
    /// температура окажется мусорной. Так же поступает эталонный драйвер
    /// (`ln8000_get_adc_data`): ставит бит паузы, читает пару, снимает бит.
    ///
    /// Пауза снимается и при ошибке чтения: иначе АЦП остался бы стоять.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — чип не открыт.
    /// * [`PumpError::Bus`] — шина не ответила.
    pub fn read_adc(&mut self, channel: AdcChannel) -> Result<i32, PumpError> {
        self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_PAUSE_ADC,
            regs::TIMER_CTRL_PAUSE_ADC,
            "adc_pause",
        )?;
        let outcome = self.read_pair(channel.register());
        // Снимаем паузу в любом случае и только потом разбираем результат.
        let _ = self.update(
            regs::TIMER_CTRL,
            regs::TIMER_CTRL_PAUSE_ADC,
            0,
            "adc_resume",
        );
        Ok(channel.decode(outcome?))
    }

    /// Выполняет программный сброс устройства.
    ///
    /// После сброса устройство возвращается в состояние «по умолчанию», поэтому
    /// сессия снова помечается как [`PumpState::Probed`] — конфигурацию нужно
    /// применить заново. Паузу [`regs::SOFT_RESET_DELAY_MS`] выдерживает
    /// вызывающая сторона.
    ///
    /// # Errors
    ///
    /// [`PumpError::Bus`] — сбой шины.
    pub fn soft_reset(&mut self) -> Result<(), PumpError> {
        self.write(regs::LION_CTRL, regs::LION_CTRL_UNLOCK)?;
        // Absolute write, no verify/readback: the soft-reset bit self-clears and
        // the chip PORs. Live nabu: `update`+`verify_writes` hung the I²C
        // controller mid-read after BC_OP_2 bit0 (IOCTL WRITE_REG / SET_CHARGE
        // paths). Caller must wait [`regs::SOFT_RESET_DELAY_MS`] then configure.
        let current = self.read(regs::BC_OP_2).unwrap_or(0);
        self.write(regs::BC_OP_2, current | (1 << 0))?;
        self.state = PumpState::Probed;
        self.op_mode = OpMode::Unknown;
        Ok(())
    }

    /// Читает регистр напрямую (диагностика).
    ///
    /// Используется служебным интерфейсом драйвера, когда нужно посмотреть
    /// регистр, для которого нет отдельного метода.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::Bus`] — сбой шины.
    pub fn read_register(&mut self, addr: u8) -> Result<u8, PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        self.read(addr)
    }

    /// Записывает регистр напрямую (диагностика), с проверкой чтением.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::OutOfRange`] — прочитанное значение не совпало с записанным.
    pub fn write_register(&mut self, addr: u8, value: u8) -> Result<(), PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        // Soft-reset via diagnostic WR must not verify (same hang as soft_reset).
        if addr == regs::BC_OP_2 && value & (1 << 0) != 0 {
            return self.write(addr, value);
        }
        self.write_verified(addr, value, "diagnostic")
    }

    /// Закрывает сессию: устройство переводится в standby.
    ///
    /// Ошибки в закрытии поглощаются: выгрузка драйвера не должна зависеть от
    /// доступности шины.
    pub fn close(&mut self) {
        if self.state == PumpState::Closed {
            return;
        }
        // Маскированно, как `ln8000_change_opmode` (маска 0x09): абсолютная
        // запись `STANDBY_EN` обнуляла биты 7:4 и 1 `SYS_CTRL`.
        let _ = self.update(
            regs::SYS_CTRL,
            OpMode::sys_ctrl_mask(),
            regs::SYS_CTRL_STANDBY_EN,
            "close_standby",
        );
        self.state = PumpState::Closed;
        self.op_mode = OpMode::Standby;
    }

    // --- внутреннее ---

    fn set_op_mode(&mut self, target: OpMode) -> Result<(), PumpError> {
        if self.state == PumpState::Closed {
            return Err(PumpError::NotOpen);
        }
        let bits = target.sys_ctrl_bits()?;
        self.update(regs::SYS_CTRL, OpMode::sys_ctrl_mask(), bits, "op_mode")?;
        self.op_mode = target;
        Ok(())
    }

    fn read(&mut self, addr: u8) -> Result<u8, PumpError> {
        let mut attempt: u8 = 0;
        loop {
            match self.bus.read(addr) {
                Ok(value) => {
                    self.reads = self.reads.saturating_add(1);
                    return Ok(value);
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !bus_recoverable(&err) || attempt > self.config.max_bus_retries {
                        self.state = PumpState::Faulted;
                        return Err(PumpError::Bus(err));
                    }
                    let _ = self.bus.reset();
                }
            }
        }
    }

    fn read_pair(&mut self, addr: u8) -> Result<u16, PumpError> {
        let mut attempt: u8 = 0;
        loop {
            match self.bus.read_pair(addr) {
                Ok(value) => {
                    self.reads = self.reads.saturating_add(2);
                    return Ok(value);
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !bus_recoverable(&err) || attempt > self.config.max_bus_retries {
                        self.state = PumpState::Faulted;
                        return Err(PumpError::Bus(err));
                    }
                    let _ = self.bus.reset();
                }
            }
        }
    }

    fn write(&mut self, addr: u8, value: u8) -> Result<(), PumpError> {
        let mut attempt: u8 = 0;
        loop {
            match self.bus.write(addr, value) {
                Ok(()) => {
                    self.writes = self.writes.saturating_add(1);
                    return Ok(());
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !bus_recoverable(&err) || attempt > self.config.max_bus_retries {
                        self.state = PumpState::Faulted;
                        return Err(PumpError::Bus(err));
                    }
                    let _ = self.bus.reset();
                }
            }
        }
    }

    fn update(
        &mut self,
        addr: u8,
        mask: u8,
        value: u8,
        field: &'static str,
    ) -> Result<(), PumpError> {
        let current = self.read(addr)?;
        let updated = (current & !mask) | (value & mask);
        self.write(addr, updated)?;
        if self.config.verify_writes {
            let read_back = self.read(addr)?;
            if read_back != updated {
                return Err(self.fault_out_of_range(field, u32::from(updated)));
            }
        }
        Ok(())
    }

    fn write_verified(
        &mut self,
        addr: u8,
        value: u8,
        field: &'static str,
    ) -> Result<(), PumpError> {
        self.write(addr, value)?;
        if self.config.verify_writes {
            let read_back = self.read(addr)?;
            if read_back != value {
                return Err(self.fault_out_of_range(field, u32::from(value)));
            }
        }
        Ok(())
    }

    fn fault_out_of_range(&mut self, field: &'static str, requested: u32) -> PumpError {
        self.state = PumpState::Faulted;
        PumpError::OutOfRange { field, requested }
    }
}

impl<T: RegisterBus> Drop for Pump<T> {
    /// Возвращает устройство в standby; ошибки поглощаются.
    fn drop(&mut self) {
        self.close();
    }
}

fn bus_recoverable(err: &BusError) -> bool {
    matches!(
        err.kind,
        crate::error::BusErrorKind::Timeout
            | crate::error::BusErrorKind::Disconnected
            | crate::error::BusErrorKind::Io
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::{GuardAction, GuardLimits, evaluate};
    use crate::session::TelemetrySample;
    use crate::testkit::{Fault, MockPumpBus};

    #[test]
    fn open_detects_wrong_chip() {
        let mut bus = MockPumpBus::new();
        bus.set_reg(regs::DEVICE_ID, 0x11);
        let err = Pump::open(bus, PumpConfig::default()).unwrap_err();
        assert!(matches!(err, PumpError::WrongDeviceId { got: 0x11 }));
    }

    #[test]
    fn open_reports_dead_bus() {
        let mut bus = MockPumpBus::new();
        bus.push_fault(Fault::ReadError {
            addr: regs::DEVICE_ID,
            times: 5,
        });
        let config = PumpConfig {
            max_bus_retries: 1,
            ..PumpConfig::default()
        };
        let err = Pump::open(bus, config).unwrap_err();
        assert_eq!(err.code(), "bus");
    }

    #[test]
    fn configure_writes_expected_registers() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        assert_eq!(pump.state(), PumpState::Configured);

        // Проверяем ключевые значения, которые обязан записать драйвер.
        assert_eq!(
            pump.bus.reg(regs::V_FLOAT_CTRL),
            encode_vbat_float(NABU_VBAT_FLOAT_UV)
        );
        assert_eq!(pump.bus.reg(regs::IIN_CTRL) & 0x7F, 40); // 2 А / 50 мА
        assert_eq!(
            pump.bus.reg(regs::THRESHOLD_CTRL),
            regs::THRESHOLD_CTRL_DEFAULT
        );
        assert_eq!(pump.bus.reg(regs::ADC_CFG), 0x3E);
        assert_eq!(pump.bus.reg(regs::CHARGE_CTRL) & (1 << 7), 1 << 7);
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_STANDBY_EN,
            1 << 3
        );
        // Петли регулирования включены (биты «disable» сняты, «int» выставлены).
        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(regulation & (1 << 5), 0);
        assert_eq!(regulation & (1 << 4), 0);
        assert_eq!(regulation & (1 << 7), 1 << 7);
        assert_eq!(regulation & (1 << 6), 1 << 6);
    }

    #[test]
    fn enable_switching_reaches_mode_three() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        assert_eq!(pump.enable_switching().unwrap(), OpMode::Switching);
        assert_eq!(pump.state(), PumpState::Switching);
        assert_eq!(pump.status().unwrap().op_mode, OpMode::Switching);
    }

    #[test]
    fn switching_is_reported_when_chip_refuses() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // Устройство игнорирует команду и остаётся в standby.
        pump.bus.push_fault(Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY,
        });
        let err = pump.enable_switching().unwrap_err();
        assert!(matches!(err, PumpError::ModeNotReached { .. }));
        assert!(err.is_recoverable());
    }

    #[test]
    fn bypass_mode_sets_1to1_bit() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        let mode = pump.enable_bypass().unwrap();
        assert_eq!(mode, OpMode::Bypass);
        assert_eq!(pump.bus.reg(regs::SYS_CTRL) & 1, 1);
    }

    #[test]
    fn iin_limit_updates_and_rejects_low_values() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        let code = pump.set_iin_limit(3_000_000).unwrap();
        assert_eq!(code, 60);
        assert_eq!(pump.bus.reg(regs::IIN_CTRL) & 0x7F, 60);
        let err = pump.set_iin_limit(10_000).unwrap_err();
        assert_eq!(err.code(), "out_of_range");
    }

    #[test]
    fn status_reports_faults() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.bus.set_reg(regs::FAULT1_STS, regs::FAULT1_WATCHDOG);
        let status = pump.status().unwrap();
        assert!(status.watchdog_expired());
        assert!(status.has_critical_fault());
        assert_eq!(status.fault_summary(), "watchdog");
    }

    #[test]
    fn adc_channels_are_read() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.bus.set_reg(AdcChannel::Vbat.register(), 0x2B);
        pump.bus.set_reg(AdcChannel::Vbat.register() + 1, 0x01);
        pump.bus.set_reg(AdcChannel::Iin.register(), 0xC8);
        // Код 0x012B = 299, шаг 5 мВ: 299 × 5 мВ = 1.495 В (смещения нет).
        assert_eq!(pump.read_adc(AdcChannel::Vbat).unwrap(), 1_495_000);
        // Код 200 → 200 × 4.89 мА = 978 мА
        assert_eq!(pump.read_adc(AdcChannel::Iin).unwrap(), 978_000);
    }

    #[test]
    fn adc_read_pauses_and_resumes_conversion_update() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        // Посторонний бит в TIMER_CTRL не должен пострадать от правки.
        pump.bus.set_reg(regs::TIMER_CTRL, 0b1000_0000);
        pump.bus.set_reg(AdcChannel::Vbat.register(), 0x2B);
        pump.bus.set_reg(AdcChannel::Vbat.register() + 1, 0x01);

        assert_eq!(pump.read_adc(AdcChannel::Vbat).unwrap(), 1_495_000);

        let timer = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(
            timer & regs::TIMER_CTRL_PAUSE_ADC,
            0,
            "после чтения пауза обновления АЦП должна быть снята"
        );
        assert_eq!(
            timer & 0b1000_0000,
            0b1000_0000,
            "посторонний бит в TIMER_CTRL должен сохраниться"
        );
    }

    #[test]
    fn switching_or_bypass_prefers_fast_mode() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        assert_eq!(
            pump.enable_switching_or_bypass().unwrap(),
            OpMode::Switching,
            "если чип подтвердил 2:1, остаёмся в нём"
        );
    }

    #[test]
    fn switching_or_bypass_falls_back_only_on_the_five_volt_side() {
        // 5 В: 2:1 не подтверждается, но 1:1 допустим — режим всё равно включается.
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        // Чип «не слышит» команду 2:1 и всегда отвечает, что он в bypass.
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_BYPASS_ENABLED,
        });
        assert_eq!(
            pump.enable_switching_or_bypass().unwrap(),
            OpMode::Bypass,
            "на 5 В при отказе 2:1 обязан включиться bypass"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            regs::SYS_CTRL_EN_1TO1,
            "в SYS_CTRL должен стоять бит 1:1"
        );

        // Повышенный Vin: отступление в 1:1 запрещено (это 9 В на батарею),
        // возвращается ошибка исходного режима, бит 1:1 не выставляется.
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_000_000);
        set_vbat_uv(&mut pump, 4_275_000);
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_BYPASS_ENABLED,
        });
        let err = pump.enable_switching_or_bypass().unwrap_err();
        assert!(matches!(err, PumpError::ModeNotReached { .. }), "{err:?}");
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 при 9 В — перенапряжение на батарее"
        );
    }

    #[test]
    fn switching_or_bypass_reports_error_when_nothing_confirmed() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY,
        });
        let err = pump.enable_switching_or_bypass().unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "ожидалась ошибка режима, получено: {err:?}"
        );
        // После такого отказа драйвер уводит чип в standby: проверим, что это возможно.
        assert!(pump.standby().is_ok(), "standby должен подтверждаться");
    }

    /// Pack Vin ADC registers so `read_adc(Vin)` returns approximately `uv`.
    fn set_vin_uv(pump: &mut Pump<MockPumpBus>, uv: i32) {
        let units = u16::try_from((uv / 16_000).clamp(0, 1023)).unwrap_or(0);
        let high = u8::try_from((units / 16) & 0x3F).unwrap_or(0);
        let low = u8::try_from((units % 16) * 16).unwrap_or(0);
        let register = AdcChannel::Vin.register();
        pump.bus_mut().set_reg(register, low);
        pump.bus_mut().set_reg(register + 1, high);
    }

    fn set_vbat_uv(pump: &mut Pump<MockPumpBus>, uv: i32) {
        let units = u16::try_from((uv / 5_000).clamp(0, 1023)).unwrap_or(0);
        let high = u8::try_from((units / 256) & 0x03).unwrap_or(0);
        let low = u8::try_from(units % 256).unwrap_or(0);
        let register = AdcChannel::Vbat.register();
        pump.bus_mut().set_reg(register, low);
        pump.bus_mut().set_reg(register + 1, high);
    }

    #[test]
    fn set_charging_uses_bypass_at_five_volts() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        assert_eq!(pump.set_charging(true, &mut || {}).unwrap(), OpMode::Bypass);
        // Чистый путь 1:1 не трогает `FAULT_CTRL`: маска — надстройка стадии 3,
        // и именно её отсутствие отличало рабочий прогон 17.09 от `.627`–`.628`.
        assert_eq!(pump.bypass_stage(), 1, "без отказа чипа POR не нужен");
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "стадия 1 не имеет права маскировать отказы"
        );
    }

    #[test]
    fn set_charging_5v_soft_resets_when_bypass_blocked() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.set_reg(regs::FAULT1_STS, 0x21);
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let mut por_delays = 0_u32;
        assert_eq!(
            pump.set_charging(true, &mut || por_delays += 1).unwrap(),
            OpMode::Bypass
        );
        assert_eq!(
            por_delays, 1,
            "5 V recovery обязан выдержать POR после soft_reset"
        );
        assert_eq!(
            pump.bypass_stage(),
            2,
            "режим подтверждён на стадии POR, а не на маске"
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "POR-путь профиля отказы не маскирует"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            regs::SYS_CTRL_EN_1TO1
        );
    }

    #[test]
    fn set_charging_5v_reports_stage_three_when_nothing_works() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.push_fault(crate::testkit::Fault::RefuseSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "отказ должен называть режим, а не шину: {err:?}"
        );
        assert_eq!(
            pump.bypass_stage(),
            3,
            "последней попыткой была маска отказов"
        );
        // Маска не помогла — на узле её быть не должно: выключенные защиты
        // после неудачной попытки опаснее самого отказа режима.
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_MASK_5V_BYPASS,
            0,
            "неудачная стадия 3 обязана вернуть FAULT_CTRL как было"
        );
    }

    #[test]
    fn set_charging_5v_does_not_reset_twice_on_one_input() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.push_fault(crate::testkit::Fault::RefuseSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let mut por_delays = 0_u32;
        assert!(pump.set_charging(true, &mut || por_delays += 1).is_err());
        assert_eq!(por_delays, 1, "первый заход имеет право на POR");
        assert!(pump.por_spent());

        // Тот же вход: второй POR запрещён — иначе драйвер дёргает заряд каждый
        // тик телеметрии и стирает законно защёлкнутое состояние.
        let _ = pump.set_charging(true, &mut || por_delays += 1);
        assert_eq!(por_delays, 1, "повторный POR на том же Vin запрещён");

        // Вход сменился в пределах 5-вольтового окна (другой блок): бюджет POR
        // открывается заново. Смена 5 В → 9 В бюджета не касается — там 1:1
        // вообще не запрашивается.
        set_vin_uv(&mut pump, 5_500_000);
        let _ = pump.set_charging(true, &mut || por_delays += 1);
        assert_eq!(por_delays, 2, "новый вход — новый POR-бюджет");
    }

    #[test]
    fn charging_off_opens_a_fresh_por_budget() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        pump.bus.push_fault(crate::testkit::Fault::RefuseSysSts {
            value: regs::SYS_STS_STANDBY | 0x20,
        });
        let mut por_delays = 0_u32;
        assert!(pump.set_charging(true, &mut || por_delays += 1).is_err());
        assert!(pump.por_spent());

        let _ = pump.set_charging(false, &mut || por_delays += 1);
        assert!(
            !pump.por_spent(),
            "выключение заряда размыкает узел: бюджет POR сбрасывается"
        );
    }

    #[test]
    fn set_charging_five_volt_path_does_not_wait_without_soft_reset() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 5_000_000);
        let mut por_delays = 0_u32;
        assert_eq!(
            pump.set_charging(true, &mut || por_delays += 1).unwrap(),
            OpMode::Bypass
        );
        assert_eq!(por_delays, 0, "без soft_reset пауза POR не нужна");
    }

    #[test]
    fn enable_bypass_refuses_elevated_vin() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // F1/F2: единый гейт внутри enable_bypass — 9 В на батарею недопустимы.
        set_vin_uv(&mut pump, 9_000_000);
        set_vbat_uv(&mut pump, 4_275_000);
        let err = pump.enable_bypass().unwrap_err();
        assert!(
            matches!(
                err,
                PumpError::BypassNeedsFiveVoltVin { vin_uv }
                    if vin_uv >= crate::encoding::SWITCHING_MIN_VIN_UV
            ),
            "{err:?}"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 bit must stay clear at 9 V"
        );
        // То же на границе 2:1 и на 12 В.
        for vin in [8_000_000, 12_000_000] {
            set_vin_uv(&mut pump, vin);
            assert!(matches!(
                pump.enable_bypass().unwrap_err(),
                PumpError::BypassNeedsFiveVoltVin { .. }
            ));
        }
        // В окне обхода режим по-прежнему включается.
        set_vin_uv(&mut pump, 5_000_000);
        assert_eq!(pump.enable_bypass().unwrap(), OpMode::Bypass);
    }

    #[test]
    fn set_charging_uses_switching_at_nine_volts() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_000_000);
        assert_eq!(
            pump.set_charging(true, &mut || {}).unwrap(),
            OpMode::Switching
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_DISABLE_VIN_OV,
            regs::FAULT_CTRL_DISABLE_VIN_OV,
            "elevated Vin must mask VIN_OV"
        );
    }

    #[test]
    fn set_charging_elevated_vin_without_headroom_stays_in_standby() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // Живой случай nabu: PD-блок 8,416 В при батарее 4,275 В.
        // 2:1 требует >= 2*4,275 + 0,25 = 8,8 В и физически не тянет.
        set_vin_uv(&mut pump, 8_416_000);
        set_vbat_uv(&mut pump, 4_275_000);
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "нет запаса по напряжению — режим не запрашивается: {err:?}"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 при повышенном Vin подал бы 8+ В на батарею"
        );
        assert_eq!(
            pump.status().unwrap().op_mode,
            OpMode::Standby,
            "неудачная попытка обязана оставить чип в standby"
        );
    }

    #[test]
    fn set_charging_never_picks_bypass_at_eight_volts() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        // 8,0 В при заряженной батарее (4,4 В): 2:1 не проходит, bypass запрещён.
        set_vin_uv(&mut pump, 8_000_000);
        set_vbat_uv(&mut pump, 4_400_000);
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(matches!(err, PumpError::ModeNotReached { .. }), "{err:?}");
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "верхняя граница обхода — SWITCHING_MIN_VIN_UV"
        );
        // Ниже 8 В bypass снова разрешён.
        set_vin_uv(&mut pump, 5_000_000);
        assert_eq!(pump.set_charging(true, &mut || {}).unwrap(), OpMode::Bypass);
    }

    #[test]
    fn set_charging_elevated_vin_does_not_fallback_to_bypass() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 12_000_000);
        pump.bus.push_fault(crate::testkit::Fault::StuckSysSts {
            value: regs::SYS_STS_STANDBY,
        });
        let err = pump.set_charging(true, &mut || {}).unwrap_err();
        assert!(
            matches!(err, PumpError::ModeNotReached { .. }),
            "must not silently bypass at 12 V: {err:?}"
        );
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_EN_1TO1,
            0,
            "1:1 bit must stay clear after failed elevated start"
        );
    }

    #[test]
    fn set_charging_near_float_soft_clears_vbat_ov() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_600_000);
        set_vbat_uv(&mut pump, 4_520_000);
        pump.bus.set_reg(regs::FAULT1_STS, regs::FAULT1_VBAT_OV);
        assert_eq!(
            pump.set_charging(true, &mut || {}).unwrap(),
            OpMode::Switching
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_DISABLE_VBAT_OV,
            regs::FAULT_CTRL_DISABLE_VBAT_OV,
            "near-float must soft-mask VBAT_OV"
        );
        assert_eq!(
            pump.bus.reg(regs::FAULT_CTRL) & regs::FAULT_CTRL_DISABLE_VIN_OV,
            regs::FAULT_CTRL_DISABLE_VIN_OV,
            "elevated Vin must still mask VIN_OV"
        );
        let float_code = pump.bus.reg(regs::V_FLOAT_CTRL);
        assert!(
            float_code >= encode_vbat_float(crate::encoding::VBAT_FLOAT_SOFT_MAX_UV),
            "soft float should reach ~4.50 V headroom, got 0x{float_code:02X}"
        );
        let iin = pump.bus.reg(regs::IIN_CTRL) & 0x7F;
        assert!(
            iin <= encode_iin_limit(crate::encoding::VBAT_TAPER_IIN_UA).unwrap(),
            "near-float must taper IIN"
        );
    }

    #[test]
    fn near_float_taper_setpoint_is_the_one_the_guard_sees() {
        // F10: тапер у верха заряда пишет 1,2 А мимо профиля (2,8 А остаётся в
        // `config`). Защита обязана видеть именно уставку из `IIN_CTRL`: по
        // профилю она «снижала» бы ток до полосы 2,0 А, то есть поднимала его
        // с 1,2 А при 44 °C и 4,46 В.
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).unwrap();
        pump.configure().unwrap();
        set_vin_uv(&mut pump, 9_600_000);
        set_vbat_uv(&mut pump, 4_460_000);
        assert_eq!(
            pump.set_charging(true, &mut || {}).unwrap(),
            OpMode::Switching
        );

        let applied = pump.applied_iin_ua().expect("IIN_CTRL читается");
        assert_eq!(
            applied, VBAT_TAPER_IIN_UA,
            "тапер должен был записать 1,2 А в чип"
        );
        assert_eq!(
            pump.config().iin_limit_ua,
            PumpConfig::for_qc35_class_b().iin_limit_ua,
            "профиль тапер не трогает — в `config` по-прежнему 2,8 А"
        );
        // Помощник — единый источник истины о намеренной уставке: он обязан
        // вернуть ровно то, что тапер записал в регистр, и молчать вне полосы.
        assert_eq!(
            pump.taper_setpoint_ua(4_460_000, 9_600_000, false),
            Some(VBAT_TAPER_IIN_UA),
            "намеренная уставка в полосе тапера — 1,2 А"
        );
        assert_eq!(
            pump.taper_setpoint_ua(4_300_000, 9_600_000, false),
            None,
            "вне полосы тапера намеренной уставки нет"
        );
        assert_eq!(
            pump.taper_setpoint_ua(4_800_000, 9_600_000, false),
            None,
            "артефакт VBAT ≈ Vin/2 тапером не считается"
        );

        let mut limits = GuardLimits::standard();
        limits.iin_profile_ua = pump.config().iin_limit_ua;
        limits.vbat_reduce_uv = crate::encoding::NABU_VBAT_NON_FFC_UV;
        // Оба условия среза сразу: температура выше порога среза и 4,46 В ≥ 4,45 В.
        // Порог берётся из профиля: он обязан лежать выше температуры покоя
        // кристалла этой платы (живой замер 19.09 — 46,1 °C в простое).
        let sample = TelemetrySample {
            ts_ms: 1_000,
            vbat_uv: 4_460_000,
            vbus_uv: 9_600_000,
            iin_ua: 1_200_000,
            die_temp_dc: limits.temp_reduce_dc + 1,
            op_mode: OpMode::Switching,
            input_present: true,
            vbat_valid: true,
            die_temp_valid: true,
        };
        assert_eq!(
            evaluate(
                &sample,
                &limits,
                pump.applied_iin_ua(),
                pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, false)
            ),
            GuardAction::None,
            "1,2 А ниже полосы среза: защита не имеет права поднимать ток"
        );
        // Тот же отсчёт, но с уставкой профиля (2,8 А) — обычный срез до полосы.
        assert_eq!(
            evaluate(&sample, &limits, Some(2_800_000), None),
            GuardAction::ReduceCurrent {
                to_ua: 2_000_000,
                reason: "die_temp_reduce",
            }
        );
    }

    #[test]
    fn clear_latched_faults_pulses_timer_bit() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.bus.set_reg(regs::TIMER_CTRL, 0xB0);
        pump.clear_latched_faults().unwrap();
        assert_eq!(
            pump.bus.reg(regs::TIMER_CTRL) & regs::TIMER_CTRL_CLEAR_LATCH,
            0
        );
        assert_eq!(pump.bus.reg(regs::TIMER_CTRL) & 0xB0, 0xB0);
    }

    #[test]
    fn watchdog_service_keeps_timer_bits() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(
            bus,
            PumpConfig {
                watchdog_enabled: true,
                watchdog_period: WatchdogPeriod::Sec10,
                ..PumpConfig::default()
            },
        )
        .unwrap();
        pump.configure().unwrap();

        let after_configure = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(
            after_configure & (1 << 7),
            1 << 7,
            "сторож должен быть включён"
        );
        assert_eq!(
            after_configure & (0b11 << 5),
            WatchdogPeriod::Sec10.code() << 5,
            "период должен быть записан в биты 5–6"
        );

        // Чип сам сбрасывает бит после срабатывания; обслуживание возвращает его
        // и не трогает период.
        pump.bus.set_reg(regs::TIMER_CTRL, 0);
        pump.service_watchdog().unwrap();
        let after_service = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(
            after_service & (1 << 7),
            1 << 7,
            "обслуживание включает сторож"
        );
        assert_eq!(
            after_service & (0b11 << 5),
            0,
            "чужие биты обслуживание не пишет"
        );
    }

    #[test]
    fn watchdog_service_is_noop_when_disabled() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        let before = pump.bus.reg(regs::TIMER_CTRL);
        assert_eq!(before & (1 << 7), 0, "по умолчанию сторож выключен");
        pump.service_watchdog().unwrap();
        assert_eq!(
            pump.bus.reg(regs::TIMER_CTRL),
            before,
            "выключенный сторож не должен включаться сам"
        );
    }

    #[test]
    fn registry_parameters_change_profile_and_reject_junk() {
        let mut config = PumpConfig::for_qc35_class_b();

        assert!(config.apply_parameter("IinLimitUa", 1_500_000));
        assert_eq!(config.iin_limit_ua, 1_500_000);
        assert!(config.apply_parameter("VbatFloatUv", 4_400_000));
        assert_eq!(config.vbat_float_uv, 4_400_000);
        assert!(config.apply_parameter("VacOvpUv", 11_000_000));
        assert_eq!(config.vac_ovp_uv, 11_000_000);
        assert!(config.apply_parameter("NtcAlarmCfg", 226));
        assert_eq!(config.ntc_alarm_cfg, 226);
        assert!(config.apply_parameter("WatchdogEnabled", 1));
        assert!(config.watchdog_enabled);

        // Значения вне границ и неизвестные имена отвергаются и ничего не портят.
        let before = config;
        assert!(!config.apply_parameter("IinLimitUa", 10));
        assert!(!config.apply_parameter("VbatFloatUv", 9_000_000));
        assert!(!config.apply_parameter("VacOvpUv", 3_000_000));
        assert!(!config.apply_parameter("NtcAlarmCfg", 0x0400));
        assert!(!config.apply_parameter("СовсемДругойПараметр", 1));
        assert_eq!(config, before, "неверные значения не должны менять профиль");
    }

    #[test]
    fn protection_profile_parameter_switches_protections() {
        let mut config = PumpConfig::for_nabu_dts();
        assert!(config.tdie_prot_disabled, "база — конфигурация планшета");

        assert!(config.apply_parameter("ProtectionProfile", 1));
        assert!(!config.tdie_prot_disabled, "профиль 1 включает защиты");
        assert!(!config.iin_ocp_disabled);

        assert!(config.apply_parameter("ProtectionProfile", 0));
        assert!(
            config.tdie_prot_disabled,
            "профиль 0 возвращает конфигурацию DTS"
        );

        assert!(
            !config.apply_parameter("ProtectionProfile", 7),
            "неизвестный профиль отвергается"
        );
    }

    #[test]
    fn qc35_profile_enables_vfloat_loop_for_windows() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::for_qc35_class_b()).unwrap();
        pump.configure().unwrap();
        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(
            regulation & (1 << 5),
            0,
            "Windows PEIC must keep VFLOAT regulation enabled"
        );
        assert_eq!(
            regulation & (1 << 4),
            0,
            "Windows PEIC must keep IIN regulation enabled"
        );
        assert_ne!(regulation & (1 << 7), 0, "vfloat loop int enabled");
        assert_ne!(regulation & (1 << 6), 0, "iin loop int enabled");
    }

    #[test]
    fn nabu_dts_profile_disables_pump_protections() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::for_nabu_dts()).unwrap();
        pump.configure().unwrap();

        // Регуляция и температурные петли отключены — так требует DTS планшета.
        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(regulation & (1 << 7), 0, "петля vfloat выключена");
        assert_eq!(regulation & (1 << 6), 0, "петля iin выключена");
        assert_eq!(regulation & (1 << 5), 1 << 5, "регуляция vfloat отключена");
        assert_eq!(regulation & (1 << 4), 1 << 4, "регуляция iin отключена");
        assert_eq!(regulation & (1 << 2), 0, "защита кристалла отключена");
        assert_eq!(regulation & (1 << 1), 0, "регуляция кристалла отключена");
        assert_eq!(
            regulation & (0b11 << 2),
            regs::NTC_SHUTDOWN_CFG << 2,
            "конфигурация NTC не пострадала"
        );

        // Аппаратные защиты напряжения остаются включёнными.
        let fault = pump.bus.reg(regs::FAULT_CTRL);
        assert_eq!(fault & (1 << 6), 1 << 6, "iin ocp отключён по DTS");
        assert_eq!(fault & (1 << 5), 0, "vbat ovp включён");
        assert_eq!(fault & (1 << 4), 0, "vac ov включён");

        // Мониторы температур шины и батареи — тоже отключены по DTS.
        let recovery = pump.bus.reg(regs::RECOVERY_CTRL);
        assert_eq!(recovery & 0b11, 0, "мониторы шины и батареи отключены");
    }

    #[test]
    fn protective_profile_enables_pump_protections() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::protective()).unwrap();
        pump.configure().unwrap();

        let regulation = pump.bus.reg(regs::REGULATION_CTRL);
        assert_eq!(regulation & (1 << 7), 1 << 7, "петля vfloat включена");
        assert_eq!(regulation & (1 << 6), 1 << 6, "петля iin включена");
        assert_eq!(regulation & (1 << 5), 0, "регуляция vfloat включена");
        assert_eq!(regulation & (1 << 4), 0, "регуляция iin включена");
        assert_eq!(regulation & (1 << 2), 1 << 2, "защита кристалла включена");
        assert_eq!(
            regulation & (1 << 1),
            1 << 1,
            "регуляция кристалла включена"
        );

        let fault = pump.bus.reg(regs::FAULT_CTRL);
        // IIN_OCP выключен в любом профиле: вендорский DT планшета его не
        // оставляет, а защёлка `FAULT2_IIN_OC` паркует насос в standby.
        assert_eq!(fault & (1 << 6), 1 << 6, "iin ocp выключен");
        let recovery = pump.bus.reg(regs::RECOVERY_CTRL);
        assert_eq!(recovery & 0b11, 0b11, "мониторы температур включены");
    }

    #[test]
    fn soft_reset_returns_to_probed_state() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.enable_switching().unwrap();
        pump.soft_reset().unwrap();
        assert_eq!(pump.state(), PumpState::Probed);
        assert_eq!(pump.bus.reg(regs::LION_CTRL), regs::LION_CTRL_UNLOCK);
        assert_eq!(pump.bus.reg(regs::BC_OP_2) & 1, 1);
        // Конфигурацию нужно применить заново.
        pump.configure().unwrap();
        assert_eq!(pump.enable_switching().unwrap(), OpMode::Switching);
    }

    #[test]
    fn diagnostic_register_access_round_trips() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.write_register(regs::GLITCH_CTRL, 0x0C).unwrap();
        assert_eq!(pump.read_register(regs::GLITCH_CTRL).unwrap(), 0x0C);
        pump.close();
        assert_eq!(
            pump.read_register(regs::GLITCH_CTRL).unwrap_err().code(),
            "not_open"
        );
    }

    #[test]
    fn close_and_drop_put_device_to_standby() {
        let bus = MockPumpBus::new();
        let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
        pump.configure().unwrap();
        pump.enable_switching().unwrap();
        pump.close();
        assert_eq!(pump.state(), PumpState::Closed);
        assert_eq!(
            pump.bus.reg(regs::SYS_CTRL) & regs::SYS_CTRL_STANDBY_EN,
            1 << 3
        );
        assert_eq!(pump.status().unwrap_err().code(), "not_open");
    }

    #[test]
    fn drop_closes_session() {
        let bus = MockPumpBus::new();
        {
            let mut pump = Pump::open(bus, PumpConfig::default()).unwrap();
            pump.configure().unwrap();
            pump.enable_switching().unwrap();
        }
        // После Drop шина должна получить команду standby.
        let mut probe = MockPumpBus::new();
        probe.set_reg(regs::SYS_CTRL, 0);
        assert!(Pump::open(probe, PumpConfig::default()).is_ok());
    }

    #[test]
    fn verify_failure_is_detected() {
        let bus = MockPumpBus::new();
        let config = PumpConfig {
            verify_writes: true,
            ..PumpConfig::default()
        };
        let mut pump = Pump::open(bus, config).unwrap();
        pump.bus.push_fault(Fault::WrongReadBack {
            addr: regs::V_FLOAT_CTRL,
            value: 0x00,
            times: 1,
        });
        let err = pump.configure().unwrap_err();
        assert_eq!(err.code(), "out_of_range");
        assert_eq!(pump.state(), PumpState::Faulted);
    }
}
