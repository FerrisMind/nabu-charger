//! Mock transport with a transaction log.
//!
//! Differs from [`charger_core::testkit::ScriptedMockTransport`] in that it
//! lives in `std` and keeps a full list of accesses: handy for integration
//! tests and for command-line demos.

use charger_core::testkit::{Fault, ScriptedMockTransport};
use charger_core::{AdapterType, ChargerTransport, TransportError, TransportErrorKind, regs};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// One access to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    /// Register read.
    Read {
        /// Address.
        addr: u16,
        /// Value.
        value: u8,
    },
    /// Register write.
    Write {
        /// Address.
        addr: u16,
        /// Value.
        value: u8,
    },
    /// Failed read.
    ReadFailed {
        /// Address.
        addr: u16,
        /// Error kind.
        kind: TransportErrorKind,
    },
    /// Failed write.
    WriteFailed {
        /// Address.
        addr: u16,
        /// Error kind.
        kind: TransportErrorKind,
    },
    /// Channel reset.
    Reset,
}

/// Mock transport: a register model plus a log of all accesses.
#[derive(Debug)]
pub struct MockTransport {
    inner: ScriptedMockTransport,
    log: RefCell<Vec<Transaction>>,
    faults: RefCell<Vec<Fault>>,
    reset_fails: RefCell<u8>,
}

impl MockTransport {
    /// Port with no power.
    #[must_use]
    pub fn detached() -> Self {
        Self::wrap(ScriptedMockTransport::detached())
    }

    /// Standard USB port.
    #[must_use]
    pub fn sdp() -> Self {
        Self::wrap(ScriptedMockTransport::sdp())
    }

    /// BC1.2 charging port.
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

    /// Arbitrary adapter.
    #[must_use]
    pub fn for_adapter(adapter: AdapterType) -> Self {
        Self::wrap(ScriptedMockTransport::for_adapter(adapter))
    }

    /// Device with an unknown detection pattern (error path check).
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

    /// Swaps the adapter on the fly (simulates a reconnect).
    pub fn set_adapter(&mut self, adapter: AdapterType) {
        self.inner.set_adapter(adapter);
    }

    /// Sets the value of a model register.
    pub fn set_reg(&mut self, addr: u16, value: u8) {
        self.inner.set_reg(addr, value);
    }

    /// Injects a fault that will fire on the next matching operation.
    pub fn push_fault(&mut self, fault: Fault) {
        self.faults.borrow_mut().push(fault);
        self.inner.push_fault(fault);
    }

    /// Makes the next `times` resets fail.
    pub fn fail_next_resets(&mut self, times: u8) {
        *self.reset_fails.borrow_mut() = times;
    }

    /// The full list of accesses.
    #[must_use]
    pub fn transactions(&self) -> Vec<Transaction> {
        self.log.borrow().clone()
    }

    /// How many times the channel was reset.
    #[must_use]
    pub fn reset_count(&self) -> u32 {
        self.inner.reset_count()
    }

    /// How many register writes there were.
    #[must_use]
    pub fn write_count(&self) -> usize {
        self.log
            .borrow()
            .iter()
            .filter(|entry| matches!(entry, Transaction::Write { .. }))
            .count()
    }

    /// The last write to the given register.
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

    /// Value of a model register.
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
            return Err(TransportError::disconnected("channel reset failed (bench)"));
        }
        let _ = self.inner.reset();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Device register map: handy for checks in tests.
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
