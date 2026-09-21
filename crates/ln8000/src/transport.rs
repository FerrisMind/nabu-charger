//! Abstraction of the I²C bus the LN8000 sits on.
//!
//! The driver core does not know how the transport is built. Implementations:
//!
//! | Implementation | Where | Purpose |
//! |---|---|---|
//! | [`crate::testkit::MockPumpBus`] | this crate | tests and demos without hardware |
//! | `SpbBus` | `crates/kmdf` | real bus: SpbCx/I²C over the `PEIC` ACPI resource |

use crate::error::BusError;
use crate::regs::RegAddr;

/// Access to the LN8000 registers.
///
/// Addressing is single-byte: the chip takes the register address in the first
/// byte of the transaction.
pub trait RegisterBus {
    /// Reads a register.
    ///
    /// # Errors
    ///
    /// [`BusError`] on any bus failure.
    fn read(&mut self, addr: RegAddr) -> Result<u8, BusError>;

    /// Writes a register.
    ///
    /// # Errors
    ///
    /// [`BusError`] on any bus failure.
    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), BusError>;

    /// Resets the state of the communication channel.
    ///
    /// # Errors
    ///
    /// [`BusError`] if the channel could not be recovered.
    fn reset(&mut self) -> Result<(), BusError>;

    /// Short transport name for the journal.
    fn name(&self) -> &'static str;

    /// Updates register bits without touching the others (read-modify-write).
    ///
    /// # Errors
    ///
    /// Propagates the read or write error.
    fn update_bits(&mut self, addr: RegAddr, mask: u8, value: u8) -> Result<(), BusError> {
        let current = self.read(addr)?;
        let updated = (current & !mask) | (value & mask);
        self.write(addr, updated)
    }

    /// Reads a 10-bit value from a pair of adjacent registers.
    ///
    /// This is how the LN8000 ADC results are laid out: the code occupies two bytes
    /// (see `ln8000_bulk_read_reg(..., 2)` in the reference driver). A transport may
    /// override the method and read the pair in a single transaction.
    ///
    /// # Errors
    ///
    /// Propagates the read error.
    fn read_pair(&mut self, addr: RegAddr) -> Result<u16, BusError> {
        let low = self.read(addr)?;
        let high = self.read(addr.wrapping_add(1))?;
        Ok(u16::from(low) | (u16::from(high) << 8))
    }
}
