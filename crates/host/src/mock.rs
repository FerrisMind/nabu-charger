//! Мок-транспорт с журналом транзакций.
//!
//! Отличается от [`charger_core::testkit::ScriptedMockTransport`] тем, что
//! живёт в `std` и ведёт полный список обращений: удобно для интеграционных
//! тестов и для демонстраций из командной строки.

use charger_core::testkit::{Fault, ScriptedMockTransport};
use charger_core::{AdapterType, ChargerTransport, TransportError, TransportErrorKind, regs};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// Одно обращение к устройству.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    /// Чтение регистра.
    Read {
        /// Адрес.
        addr: u16,
        /// Значение.
        value: u8,
    },
    /// Запись регистра.
    Write {
        /// Адрес.
        addr: u16,
        /// Значение.
        value: u8,
    },
    /// Неудачное чтение.
    ReadFailed {
        /// Адрес.
        addr: u16,
        /// Категория ошибки.
        kind: TransportErrorKind,
    },
    /// Неудачная запись.
    WriteFailed {
        /// Адрес.
        addr: u16,
        /// Категория ошибки.
        kind: TransportErrorKind,
    },
    /// Сброс канала.
    Reset,
}

/// Мок-транспорт: модель регистров плюс запись всех обращений.
#[derive(Debug)]
pub struct MockTransport {
    inner: ScriptedMockTransport,
    log: RefCell<Vec<Transaction>>,
    faults: RefCell<Vec<Fault>>,
    reset_fails: RefCell<u8>,
}

impl MockTransport {
    /// Порт без питания.
    #[must_use]
    pub fn detached() -> Self {
        Self::wrap(ScriptedMockTransport::detached())
    }

    /// Стандартный порт USB.
    #[must_use]
    pub fn sdp() -> Self {
        Self::wrap(ScriptedMockTransport::sdp())
    }

    /// Порт зарядки BC1.2.
    #[must_use]
    pub fn dcp() -> Self {
        Self::wrap(ScriptedMockTransport::dcp())
    }

    /// Quick Charge 2.0.
    #[must_use]
    pub fn hvdcp2() -> Self {
        Self::wrap(ScriptedMockTransport::hvdcp2())
    }

    /// Quick Charge 3.0.
    #[must_use]
    pub fn hvdcp3() -> Self {
        Self::wrap(ScriptedMockTransport::hvdcp3())
    }

    /// Quick Charge 3.5.
    #[must_use]
    pub fn hvdcp3p5() -> Self {
        Self::wrap(ScriptedMockTransport::hvdcp3p5())
    }

    /// Произвольный адаптер.
    #[must_use]
    pub fn for_adapter(adapter: AdapterType) -> Self {
        Self::wrap(ScriptedMockTransport::for_adapter(adapter))
    }

    /// Устройство с неизвестным образцом детекции (проверка ошибочного пути).
    #[must_use]
    pub fn unknown_pattern() -> Self {
        Self::wrap(ScriptedMockTransport::unknown_pattern())
    }

    fn wrap(inner: ScriptedMockTransport) -> Self {
        Self {
            inner,
            log: RefCell::new(Vec::new()),
            faults: RefCell::new(Vec::new()),
            reset_fails: RefCell::new(0),
        }
    }

    /// Подменяет адаптер на лету (имитация переподключения).
    pub fn set_adapter(&mut self, adapter: AdapterType) {
        self.inner.set_adapter(adapter);
    }

    /// Задаёт значение регистра модели.
    pub fn set_reg(&mut self, addr: u16, value: u8) {
        self.inner.set_reg(addr, value);
    }

    /// Вносит сбой, который сработает на следующей подходящей операции.
    pub fn push_fault(&mut self, fault: Fault) {
        self.faults.borrow_mut().push(fault);
        self.inner.push_fault(fault);
    }

    /// Делает ближайшие `times` сбросов неудачными.
    pub fn fail_next_resets(&mut self, times: u8) {
        *self.reset_fails.borrow_mut() = times;
    }

    /// Полный список обращений.
    #[must_use]
    pub fn transactions(&self) -> Vec<Transaction> {
        self.log.borrow().clone()
    }

    /// Сколько раз канал сбрасывали.
    #[must_use]
    pub fn reset_count(&self) -> u32 {
        self.inner.reset_count()
    }

    /// Сколько было записей регистров.
    #[must_use]
    pub fn write_count(&self) -> usize {
        self.log
            .borrow()
            .iter()
            .filter(|entry| matches!(entry, Transaction::Write { .. }))
            .count()
    }

    /// Последняя запись в указанный регистр.
    #[must_use]
    pub fn last_write(&self, addr: u16) -> Option<u8> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Transaction::Write { addr: a, value } if *a == addr => Some(*value),
                _ => None,
            })
    }

    /// Значение регистра модели.
    #[must_use]
    pub fn reg(&self, addr: u16) -> u8 {
        self.inner.reg(addr)
    }
}

impl ChargerTransport for MockTransport {
    fn read(&mut self, addr: u16) -> Result<u8, TransportError> {
        match self.inner.read(addr) {
            Ok(value) => {
                self.log
                    .borrow_mut()
                    .push(Transaction::Read { addr, value });
                Ok(value)
            }
            Err(err) => {
                self.log.borrow_mut().push(Transaction::ReadFailed {
                    addr,
                    kind: err.kind,
                });
                Err(err)
            }
        }
    }

    fn write(&mut self, addr: u16, value: u8) -> Result<(), TransportError> {
        match self.inner.write(addr, value) {
            Ok(()) => {
                self.log
                    .borrow_mut()
                    .push(Transaction::Write { addr, value });
                Ok(())
            }
            Err(err) => {
                self.log.borrow_mut().push(Transaction::WriteFailed {
                    addr,
                    kind: err.kind,
                });
                Err(err)
            }
        }
    }

    fn reset(&mut self) -> Result<(), TransportError> {
        self.log.borrow_mut().push(Transaction::Reset);
        let mut fails = self.reset_fails.borrow_mut();
        if *fails > 0 {
            *fails = fails.saturating_sub(1);
            return Err(TransportError::disconnected(
                "сброс канала не удался (стенд)",
            ));
        }
        let _ = self.inner.reset();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Карта регистров устройства: удобно для проверок в тестах.
#[must_use]
pub fn smb_register_map() -> BTreeMap<u16, &'static str> {
    BTreeMap::from([
        (regs::APSD_STATUS, "APSD_STATUS"),
        (regs::APSD_RESULT_STATUS, "APSD_RESULT_STATUS"),
        (regs::QC_CHANGE_STATUS, "QC_CHANGE_STATUS"),
        (regs::CMD_APSD, "CMD_APSD"),
        (regs::CMD_ICL_OVERRIDE, "CMD_ICL_OVERRIDE"),
        (regs::HVDCP_PULSE_COUNT_MAX, "HVDCP_PULSE_COUNT_MAX"),
        (regs::USBIN_CURRENT_LIMIT_CFG, "USBIN_CURRENT_LIMIT_CFG"),
        (regs::USBIN_ICL_OPTIONS, "USBIN_ICL_OPTIONS"),
    ])
}
