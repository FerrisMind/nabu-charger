//! Эталонный мок шины I²C с моделью поведения чипа.
//!
//! Мок не просто хранит регистры: он **моделирует смену режима** — запись в
//! `SYS_CTRL` меняет биты в `SYS_STS` так же, как это делает LN8000. Благодаря
//! этому тесты проверяют реальный сценарий, а не заглушку.
//!
//! Модуль включается фичей `testkit` и в боевую сборку драйвера не попадает.

use crate::error::{BusError, BusErrorKind};
use crate::regs;
use crate::transport::RegisterBus;
use core::cell::Cell;

const MAX_REGS: usize = 64;
const MAX_FAULTS: usize = 8;

/// Внесённый сбой или особенность поведения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Следующие `times` чтений регистра падают.
    ReadError {
        /// Адрес регистра.
        addr: u8,
        /// Сколько раз повторить.
        times: u8,
    },
    /// Следующие `times` записей регистра падают.
    WriteError {
        /// Адрес регистра.
        addr: u8,
        /// Сколько раз повторить.
        times: u8,
    },
    /// Чтение возвращает другое значение (проверка верификации записи).
    WrongReadBack {
        /// Адрес регистра.
        addr: u8,
        /// Что вернуть.
        value: u8,
        /// Сколько раз повторить.
        times: u8,
    },
    /// `SYS_STS` зафиксирован: чип «не слышит» команду смены режима.
    StuckSysSts {
        /// Какое значение всегда возвращать.
        value: u8,
    },
}

impl Fault {
    const fn times(self) -> u8 {
        match self {
            Self::ReadError { times, .. }
            | Self::WriteError { times, .. }
            | Self::WrongReadBack { times, .. } => times,
            Self::StuckSysSts { .. } => u8::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FaultSlot {
    fault: Option<Fault>,
    remaining: u8,
}

/// Мок шины I²C для LN8000.
#[derive(Debug)]
pub struct MockPumpBus {
    regs: [(u8, u8); MAX_REGS],
    count: usize,
    faults: [FaultSlot; MAX_FAULTS],
    fault_count: usize,
    reset_count: Cell<u32>,
}

impl Default for MockPumpBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPumpBus {
    /// Создаёт мок с корректным идентификатором устройства и нулевым состоянием.
    #[must_use]
    pub fn new() -> Self {
        let mut bus = Self {
            regs: [(0, 0); MAX_REGS],
            count: 0,
            faults: [FaultSlot {
                fault: None,
                remaining: 0,
            }; MAX_FAULTS],
            fault_count: 0,
            reset_count: Cell::new(0),
        };
        bus.set_reg(regs::DEVICE_ID, regs::DEVICE_ID_VALUE);
        bus.set_reg(regs::SYS_STS, regs::SYS_STS_STANDBY);
        bus
    }

    /// Задаёт значение регистра модели.
    pub fn set_reg(&mut self, addr: u8, value: u8) {
        for index in 0..self.count {
            if let Some(slot) = self.regs.get_mut(index) {
                if slot.0 == addr {
                    slot.1 = value;
                    return;
                }
            }
        }
        if self.count < MAX_REGS {
            if let Some(slot) = self.regs.get_mut(self.count) {
                *slot = (addr, value);
                self.count = self.count.saturating_add(1);
            }
        }
    }

    /// Значение регистра модели.
    #[must_use]
    pub fn reg(&self, addr: u8) -> u8 {
        self.regs
            .iter()
            .take(self.count)
            .find(|slot| slot.0 == addr)
            .map_or(0, |slot| slot.1)
    }

    /// Вносит сбой или особое поведение.
    pub fn push_fault(&mut self, fault: Fault) {
        if self.fault_count < MAX_FAULTS {
            if let Some(slot) = self.faults.get_mut(self.fault_count) {
                slot.fault = Some(fault);
                slot.remaining = fault.times();
                self.fault_count = self.fault_count.saturating_add(1);
            }
        }
    }

    /// Сколько раз шину сбрасывали.
    #[must_use]
    pub fn reset_count(&self) -> u32 {
        self.reset_count.get()
    }

    fn consume<F>(&mut self, predicate: F) -> Option<Fault>
    where
        F: Fn(&Fault) -> bool,
    {
        for index in 0..self.fault_count {
            let slot = self.faults.get_mut(index)?;
            let matches = slot
                .fault
                .is_some_and(|f| predicate(&f) && slot.remaining > 0);
            if !matches {
                continue;
            }
            if matches!(slot.fault, Some(Fault::StuckSysSts { .. })) {
                return slot.fault;
            }
            slot.remaining = slot.remaining.saturating_sub(1);
            return slot.fault;
        }
        None
    }

    /// Пересчитывает `SYS_STS` так, как это делает настоящий чип.
    fn apply_sys_ctrl(&mut self, value: u8) {
        let sys_sts = if value & regs::SYS_CTRL_STANDBY_EN != 0 {
            regs::SYS_STS_STANDBY
        } else if value & regs::SYS_CTRL_EN_1TO1 != 0 {
            regs::SYS_STS_BYPASS_ENABLED
        } else {
            regs::SYS_STS_SWITCHING_ENABLED
        };
        self.set_reg(regs::SYS_STS, sys_sts);
    }

    fn clear_stuck_sys_sts_faults(&mut self) {
        for index in 0..self.fault_count {
            if let Some(slot) = self.faults.get_mut(index) {
                if matches!(slot.fault, Some(Fault::StuckSysSts { .. })) {
                    slot.fault = None;
                    slot.remaining = 0;
                }
            }
        }
    }
}

impl RegisterBus for MockPumpBus {
    fn read(&mut self, addr: u8) -> Result<u8, BusError> {
        if let Some(fault) =
            self.consume(|f| matches!(f, Fault::ReadError { addr: a, .. } if *a == addr))
        {
            let _ = fault;
            return Err(BusError::new(
                BusErrorKind::Timeout,
                0,
                "внесённый сбой чтения",
            ));
        }
        if addr == regs::SYS_STS {
            if let Some(Fault::StuckSysSts { value }) =
                self.consume(|f| matches!(f, Fault::StuckSysSts { .. }))
            {
                return Ok(value);
            }
        }
        if let Some(Fault::WrongReadBack { value, .. }) =
            self.consume(|f| matches!(f, Fault::WrongReadBack { addr: a, .. } if *a == addr))
        {
            return Ok(value);
        }
        Ok(self.reg(addr))
    }

    fn write(&mut self, addr: u8, value: u8) -> Result<(), BusError> {
        if let Some(_fault) =
            self.consume(|f| matches!(f, Fault::WriteError { addr: a, .. } if *a == addr))
        {
            return Err(BusError::new(BusErrorKind::Io, 0, "внесённый сбой записи"));
        }
        self.set_reg(addr, value);
        if addr == regs::SYS_CTRL {
            self.apply_sys_ctrl(value);
        }
        // Soft-reset (LION unlock + BC_OP_2 bit0) clears latched mode refusal,
        // matching live LN8000: FAULT1 VFAULTS drop and SYS_CTRL is honoured again.
        if addr == regs::BC_OP_2 && value & (1 << 0) != 0 {
            self.clear_stuck_sys_sts_faults();
            self.set_reg(regs::FAULT1_STS, 0);
            self.set_reg(regs::SYS_STS, regs::SYS_STS_STANDBY);
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), BusError> {
        self.reset_count
            .set(self.reset_count.get().saturating_add(1));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}
