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
    AdcHibernateDelay, AdcMode, OpMode, WatchdogPeriod, encode_iin_limit, encode_ntc_alarm,
    encode_vac_ovp, encode_vbat_float,
};
use crate::error::{BusError, PumpError};
use crate::regs;
use crate::status::{AdcChannel, Status};
use crate::transport::RegisterBus;

/// Настройки драйвера.
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
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            // Значения по умолчанию — из `ln8000_charger.h`
            // (`LN8000_BAT_OVP_DEFAULT`, `LN8000_BUS_OVP_DEFAULT`,
            // `LN8000_IIN_CFG_DEFAULT`, `LN8000_NTC_ALARM_CFG_DEFAULT`).
            vbat_float_uv: 4_440_000,
            vac_ovp_uv: 9_500_000,
            iin_limit_ua: 2_000_000,
            ntc_alarm_cfg: regs::NTC_ALARM_DEFAULT,
            watchdog_enabled: false,
            watchdog_period: WatchdogPeriod::Sec10,
            auto_recovery: false,
            verify_writes: true,
            max_bus_retries: 2,
        }
    }
}

impl PumpConfig {
    /// Профиль для работы через charge pump от блока Quick Charge 3.5 класса B.
    ///
    /// Пороги соответствуют `BUS_OVP_FOR_QC`, `BUS_OCP_FOR_QC3P5_CLASS_B`
    /// из `ln8000_charger.h`.
    #[must_use]
    pub fn for_qc35_class_b() -> Self {
        Self {
            vac_ovp_uv: 13_000_000,
            iin_limit_ua: 3_500_000 - 700_000,
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

        // 6. автовосстановление
        let recovery = if self.config.auto_recovery {
            0xF0
        } else {
            0x00
        };
        self.update(regs::RECOVERY_CTRL, 0xF0, recovery, "auto_recovery")?;

        // 7. защиты и петли регулирования
        self.update(regs::FAULT_CTRL, 1 << 5, 0, "enable_vbat_ovp")?;
        self.update(regs::FAULT_CTRL, 1 << 6, 0, "enable_iin_ocp")?;
        self.update(regs::REGULATION_CTRL, 1 << 5, 0, "enable_vfloat_loop")?;
        self.update(regs::REGULATION_CTRL, 1 << 7, 1 << 7, "vfloat_loop_int")?;
        self.update(regs::REGULATION_CTRL, 1 << 4, 0, "enable_iin_loop")?;
        self.update(regs::REGULATION_CTRL, 1 << 6, 1 << 6, "iin_loop_int")?;
        self.update(regs::REGULATION_CTRL, 1 << 2, 1 << 2, "tdie_prot")?;
        self.update(regs::REGULATION_CTRL, 1 << 1, 1 << 1, "tdie_regulation")?;
        self.update(regs::SYS_CTRL, 1 << 2, 0, "disable_reverse_current")?;

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

    /// Включает режим 2:1 (switching) и проверяет, что устройство его приняло.
    ///
    /// # Errors
    ///
    /// * [`PumpError::NotOpen`] — сессия закрыта.
    /// * [`PumpError::ModeNotReached`] — `SYS_STS` не подтвердил режим.
    pub fn enable_switching(&mut self) -> Result<OpMode, PumpError> {
        self.set_op_mode(OpMode::Switching)?;
        let status = self.status()?;
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
    /// # Errors
    ///
    /// Пробрасывает ошибки шины.
    pub fn enable_bypass(&mut self) -> Result<OpMode, PumpError> {
        self.set_op_mode(OpMode::Bypass)?;
        Ok(self.status()?.op_mode)
    }

    /// Переводит устройство в standby.
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибки шины.
    pub fn standby(&mut self) -> Result<(), PumpError> {
        self.set_op_mode(OpMode::Standby)
    }

    /// Обновляет лимит входного тока.
    ///
    /// # Errors
    ///
    /// * [`PumpError::OutOfRange`] — ток меньше [`regs::IIN_MIN_UA`].
    /// * [`PumpError::Bus`] — сбой шины.
    pub fn set_iin_limit(&mut self, iin_ua: u32) -> Result<u8, PumpError> {
        let code = encode_iin_limit(iin_ua)?;
        self.update(regs::IIN_CTRL, 0x7F, code, "iin_limit")?;
        self.config.iin_limit_ua = iin_ua;
        Ok(code)
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
    pub fn read_adc(&mut self, channel: AdcChannel) -> Result<i32, PumpError> {
        let raw = self.read_pair(channel.register())?;
        Ok(channel.decode(raw))
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
        self.update(regs::BC_OP_2, 1 << 0, 1 << 0, "soft_reset")?;
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
        let _ = self.write(regs::SYS_CTRL, regs::SYS_CTRL_STANDBY_EN);
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
            encode_vbat_float(4_440_000)
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
        // Код 0x012B = 299 → 1 В + 299 × 5 мВ = 2.495 В
        assert_eq!(pump.read_adc(AdcChannel::Vbat).unwrap(), 2_495_000);
        // Код 200 → 200 × 4.89 мА = 978 мА
        assert_eq!(pump.read_adc(AdcChannel::Iin).unwrap(), 978_000);
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
