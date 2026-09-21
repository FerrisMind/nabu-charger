//! Transport abstraction: how the core reaches the PMIC registers.
//!
//! The core knows only this trait. Implementations:
//!
//! | Implementation | Where it lives | Purpose |
//! |---|---|---|
//! | [`crate::testkit::ScriptedMockTransport`] | this crate | tests and demos without hardware |
//! | `MockTransport` | `host` | mock with a transaction journal |
//! | `TcpTransport` | `host` | real transport over the network (bench, emulator) |
//! | `SpmiTransport` | `kmdf` | real hardware: SPMI through `\Device\RESOURCE_HUB` |

use crate::error::TransportError;
use crate::regs::RegAddr;

/// Access to the charger peripheral registers.
///
/// The implementation must tolerate repeated calls after a failure: the core
/// calls [`ChargerTransport::reset`] and continues working without restarting
/// the process.
pub trait ChargerTransport {
    /// Reads one byte of a register.
    ///
    /// # Errors
    ///
    /// Any hardware access error is returned as [`TransportError`].
    fn read(&mut self, addr: RegAddr) -> Result<u8, TransportError>;

    /// Writes one byte of a register.
    ///
    /// # Errors
    ///
    /// Any hardware access error is returned as [`TransportError`].
    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError>;

    /// Resets the link state (reopens the device, clears the buffers).
    ///
    /// Called by the core when recovering from a transport failure.
    ///
    /// # Errors
    ///
    /// [`TransportError`] if the link could not be recovered.
    fn reset(&mut self) -> Result<(), TransportError>;

    /// Short transport name for the journal (for example `mock`, `tcp`, `spmi`).
    fn name(&self) -> &'static str;

    /// Reads a register with a mask: returns only the significant bits.
    ///
    /// # Errors
    ///
    /// Propagates the [`ChargerTransport::read`] error.
    fn read_masked(&mut self, addr: RegAddr, mask: u8) -> Result<u8, TransportError> {
        Ok(self.read(addr)? & mask)
    }

    /// Updates register bits without touching the rest (read-modify-write).
    ///
    /// # Errors
    ///
    /// Propagates a read or write error.
    fn update_bits(&mut self, addr: RegAddr, mask: u8, value: u8) -> Result<(), TransportError> {
        let current = self.read(addr)?;
        let updated = (current & !mask) | (value & mask);
        self.write(addr, updated)
    }
}
