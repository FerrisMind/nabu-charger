//! Reference tools for tests and demonstrations: a mock transport and an in-memory
//! journal.
//!
//! The module is enabled by the `testkit` feature (or automatically in tests) and does
//! not go into the production driver build. Everything is built on fixed buffers: no
//! allocations, no `unsafe`.

use crate::apsd::AdapterType;
use crate::error::{TransportError, TransportErrorKind};
use crate::journal::{Event, Journal};
use crate::regs;
use crate::transport::ChargerTransport;
use core::cell::{Cell, RefCell};

const MAX_REGS: usize = 64;
const MAX_FAULTS: usize = 16;
const MAX_LOG: usize = 128;

/// Injected fault: exercising error paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The next `times` reads of the given register fail with an error.
    ReadError {
        /// Register address.
        addr: u16,
        /// Error category.
        kind: TransportErrorKind,
        /// How many times to repeat.
        times: u8,
    },
    /// The next `times` writes of the given register fail with an error.
    WriteError {
        /// Register address.
        addr: u16,
        /// Error category.
        kind: TransportErrorKind,
        /// How many times to repeat.
        times: u8,
    },
    /// The link drops for `times` read operations.
    Disconnect {
        /// How many operations in a row fail.
        times: u8,
    },
    /// After a write, the read returns a different value (checks `VerifyFailed`).
    WrongReadBack {
        /// Register address.
        addr: u16,
        /// What to return on read.
        value: u8,
        /// How many times to repeat.
        times: u8,
    },
}

impl Fault {
    /// How many operations the fault lasts for.
    #[must_use]
    pub const fn times(self) -> u8 {
        match self {
            Self::ReadError { times, .. }
            | Self::WriteError { times, .. }
            | Self::Disconnect { times }
            | Self::WrongReadBack { times, .. } => times,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FaultSlot {
    fault: Option<Fault>,
    remaining: u8,
}

/// Mock transport with programmable registers and faults.
///
/// # Examples
///
/// ```
/// use core::testkit::{Fault, ScriptedMockTransport};
/// use core::{AdapterType, ChargerTransport, TransportErrorKind};
///
/// let mut mock = ScriptedMockTransport::dcp();
/// mock.push_fault(Fault::ReadError {
///     addr: 0x1307,
///     kind: TransportErrorKind::Timeout,
///     times: 1,
/// });
/// assert!(mock.read(0x1307).is_err());
/// assert_eq!(mock.read(0x1308).unwrap() & 0x7F, AdapterType::Dcp.apsd_pattern());
/// ```
#[derive(Debug)]
pub struct ScriptedMockTransport {
    regs: [(u16, u8); MAX_REGS],
    reg_count: usize,
    write_log: [(u16, u8); MAX_LOG],
    write_count: usize,
    faults: [FaultSlot; MAX_FAULTS],
    fault_count: usize,
    reset_count: Cell<u32>,
    ready_after_reads: Cell<u8>,
    status_reads: Cell<u8>,
}

impl Default for ScriptedMockTransport {
    fn default() -> Self {
        Self {
            regs: [(0, 0); MAX_REGS],
            reg_count: 0,
            write_log: [(0, 0); MAX_LOG],
            write_count: 0,
            faults: [FaultSlot {
                fault: None,
                remaining: 0,
            }; MAX_FAULTS],
            fault_count: 0,
            reset_count: Cell::new(0),
            ready_after_reads: Cell::new(0),
            status_reads: Cell::new(0),
        }
    }
}

impl ScriptedMockTransport {
    /// Port with no power: detection does not complete.
    #[must_use]
    pub fn detached() -> Self {
        Self::default()
    }

    /// Standard USB port (500 mA).
    #[must_use]
    pub fn sdp() -> Self {
        Self::for_adapter(AdapterType::Sdp)
    }

    /// Charging port (1.5 A).
    #[must_use]
    pub fn dcp() -> Self {
        Self::for_adapter(AdapterType::Dcp)
    }

    /// Quick Charge 2.0 (1.5 A, 9 V).
    #[must_use]
    pub fn hvdcp2() -> Self {
        Self::for_adapter(AdapterType::Hvdcp2)
    }

    /// Quick Charge 3.0 (3 A, 9 V).
    #[must_use]
    pub fn hvdcp3() -> Self {
        Self::for_adapter(AdapterType::Hvdcp3)
    }

    /// Quick Charge 3.5 (the tablet's own power brick).
    #[must_use]
    pub fn hvdcp3p5() -> Self {
        Self::for_adapter(AdapterType::Hvdcp3P5)
    }

    /// Unknown combination of bits in the detection result.
    #[must_use]
    pub fn unknown_pattern() -> Self {
        let mut mock = Self::default();
        mock.set_reg(regs::APSD_STATUS, regs::APSD_DTC_STATUS_DONE);
        mock.set_reg(regs::APSD_RESULT_STATUS, 0x3F);
        mock
    }

    /// Mock that reports the given adapter type.
    #[must_use]
    pub fn for_adapter(adapter: AdapterType) -> Self {
        let mut mock = Self::default();
        mock.set_adapter(adapter);
        mock
    }

    /// Programs the detection registers for the given adapter.
    pub fn set_adapter(&mut self, adapter: AdapterType) {
        let mut status = regs::APSD_DTC_STATUS_DONE;
        if adapter.is_hvdcp() {
            status |= regs::QC_CHARGER;
        }
        self.set_reg(regs::APSD_STATUS, status);
        self.set_reg(regs::APSD_RESULT_STATUS, adapter.apsd_pattern());
        self.status_reads.set(0);
    }

    /// Readiness delay: the result appears only after `reads` reads of `APSD_STATUS`.
    ///
    /// Needed to exercise the [`crate::Detection::Pending`] branch and timeouts.
    pub fn set_detection_ready_after(&self, reads: u8) {
        self.ready_after_reads.set(reads);
    }

    /// Sets a register value.
    pub fn set_reg(&mut self, addr: u16, value: u8) {
        for index in 0..self.reg_count {
            if let Some(slot) = self.regs.get_mut(index) {
                if slot.0 == addr {
                    slot.1 = value;
                    return;
                }
            }
        }
        if self.reg_count < MAX_REGS {
            if let Some(slot) = self.regs.get_mut(self.reg_count) {
                *slot = (addr, value);
                self.reg_count = self.reg_count.saturating_add(1);
            }
        }
    }

    /// Current value of the mock register.
    #[must_use]
    pub fn reg(&self, addr: u16) -> u8 {
        self.regs
            .iter()
            .take(self.reg_count)
            .find(|slot| slot.0 == addr)
            .map_or(0, |slot| slot.1)
    }

    /// Injects a fault.
    pub fn push_fault(&mut self, fault: Fault) {
        if self.fault_count < MAX_FAULTS {
            if let Some(slot) = self.faults.get_mut(self.fault_count) {
                slot.fault = Some(fault);
                slot.remaining = fault.times();
                self.fault_count = self.fault_count.saturating_add(1);
            }
        }
    }

    /// Journal of records in execution order.
    #[must_use]
    pub fn write_log(&self) -> &[(u16, u8)] {
        self.write_log.get(..self.write_count).unwrap_or(&[])
    }

    /// How many times the transport was reset.
    #[must_use]
    pub fn reset_count(&self) -> u32 {
        self.reset_count.get()
    }

    /// Finds an active fault of the given kind and consumes one of its attempts.
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
            slot.remaining = slot.remaining.saturating_sub(1);
            return slot.fault;
        }
        None
    }

    fn take_read_fault(&mut self, addr: u16) -> Option<TransportError> {
        match self.consume(|f| matches!(f, Fault::ReadError { addr: a, .. } if *a == addr)) {
            Some(Fault::ReadError { kind, .. }) => {
                Some(TransportError::new(kind, 0, "injected read fault"))
            }
            Some(Fault::Disconnect { .. }) => Some(TransportError::disconnected("link lost")),
            _ => None,
        }
    }

    fn take_write_fault(&mut self, addr: u16) -> Option<TransportError> {
        match self.consume(|f| matches!(f, Fault::WriteError { addr: a, .. } if *a == addr)) {
            Some(Fault::WriteError { kind, .. }) => {
                Some(TransportError::new(kind, 0, "injected write fault"))
            }
            _ => None,
        }
    }

    fn take_wrong_read_back(&mut self, addr: u16) -> Option<u8> {
        match self.consume(|f| matches!(f, Fault::WrongReadBack { addr: a, .. } if *a == addr)) {
            Some(Fault::WrongReadBack { value, .. }) => Some(value),
            _ => None,
        }
    }
}

impl ChargerTransport for ScriptedMockTransport {
    fn read(&mut self, addr: u16) -> Result<u8, TransportError> {
        if let Some(err) = self.take_read_fault(addr) {
            return Err(err);
        }
        if addr == regs::APSD_STATUS {
            let reads = self.status_reads.get().saturating_add(1);
            self.status_reads.set(reads);
            if reads <= self.ready_after_reads.get() {
                // Detection has not completed yet: the ready bit is clear.
                return Ok(self.reg(addr) & !regs::APSD_DTC_STATUS_DONE);
            }
        }
        if let Some(fake) = self.take_wrong_read_back(addr) {
            return Ok(fake);
        }
        Ok(self.reg(addr))
    }

    fn write(&mut self, addr: u16, value: u8) -> Result<(), TransportError> {
        if let Some(err) = self.take_write_fault(addr) {
            return Err(err);
        }
        self.set_reg(addr, value);
        if self.write_count < MAX_LOG {
            if let Some(slot) = self.write_log.get_mut(self.write_count) {
                *slot = (addr, value);
                self.write_count = self.write_count.saturating_add(1);
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), TransportError> {
        self.reset_count
            .set(self.reset_count.get().saturating_add(1));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// In-memory journal: collects events for checks without allocations.
#[derive(Debug)]
pub struct VecJournal {
    events: RefCell<[Option<Event>; MAX_LOG]>,
    count: Cell<usize>,
}

impl Default for VecJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl VecJournal {
    /// Creates an empty journal.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: RefCell::new([None; MAX_LOG]),
            count: Cell::new(0),
        }
    }

    /// How many records were collected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count.get()
    }

    /// Whether the journal is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record by index.
    #[must_use]
    pub fn at(&self, index: usize) -> Option<Event> {
        self.events.borrow().get(index).copied().flatten()
    }

    /// Calls the closure for every record in arrival order.
    pub fn for_each(&self, mut f: impl FnMut(usize, &Event)) {
        let guard = self.events.borrow();
        for (index, slot) in guard.iter().take(self.len()).enumerate() {
            if let Some(event) = slot {
                f(index, event);
            }
        }
    }

    /// First record of the given kind.
    #[must_use]
    pub fn first_of(&self, kind: &str) -> Option<Event> {
        let found: Cell<Option<Event>> = Cell::new(None);
        self.for_each(|_, event| {
            if found.get().is_none() && event.kind.name() == kind {
                found.set(Some(*event));
            }
        });
        found.get()
    }

    /// How many records of the given kind.
    #[must_use]
    pub fn count_of(&self, kind: &str) -> usize {
        let count = Cell::new(0_usize);
        self.for_each(|_, event| {
            if event.kind.name() == kind {
                count.set(count.get().saturating_add(1));
            }
        });
        count.get()
    }

    /// Copy of all records (only with `std`).
    #[cfg(feature = "std")]
    #[must_use]
    pub fn all(&self) -> Vec<Event> {
        let guard = self.events.borrow();
        guard
            .iter()
            .take(self.len())
            .filter_map(|slot| *slot)
            .collect()
    }
}

impl Journal for VecJournal {
    fn event(&self, event: &Event) {
        let index = self.count.get();
        let mut guard = self.events.borrow_mut();
        if let Some(slot) = guard.get_mut(index) {
            *slot = Some(*event);
            self.count.set(index.saturating_add(1));
        }
    }
}
