//! Charging driver core for the Xiaomi Pad 5 (`nabu`) on Windows on ARM.
//!
//! The crate is platform-independent, does not require `std` and performs no
//! input/output of its own: hardware access goes through [`ChargerTransport`], time
//! through [`Clock`], the journal through [`Journal`]. This makes all adapter detection
//! and current limit logic testable on a mock without real hardware.
//!
//! # Why this is needed
//!
//! Under Windows the tablet does not charge from any power brick: the hardware adapter
//! detection (APSD) in the PMIC does run, but no Windows component reads its result,
//! so the input current is never raised. The core closes exactly this gap: it reads the
//! detection result and applies the current policy.
//!
//! # What the core does
//!
//! 1. Verifies the link to the charger peripheral ([`Charger::open`]).
//! 2. Waits for APSD to finish and parses the adapter type ([`Charger::detect_step`]).
//! 3. Computes and applies the input current limit ([`Charger::apply`]).
//! 4. Watches for an adapter change ([`Charger::monitor`]).
//! 5. Releases resources cleanly ([`Charger::close`] and [`Drop`]).
//!
//! # Examples
//!
//! ```no_run
//! use core::testkit::ScriptedMockTransport;
//! use core::{AdapterType, Charger, ChargerConfig, ManualClock, NullJournal};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transport = ScriptedMockTransport::hvdcp3();
//! let clock = ManualClock::new();
//! let journal = NullJournal;
//!
//! let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::default())?;
//! clock.advance_ms(1_000);
//! if let core::Detection::Ready(adapter) = charger.detect_step()? {
//!     assert_eq!(adapter, AdapterType::Hvdcp3);
//!     charger.apply(adapter)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Scope of responsibility
//!
//! The crate does **not** write to the Windows registry, does not drive the charge pump
//! (LN8000) and does not touch the file system. The transport to the PMIC registers is
//! provided by the layer above: in the host tools it is a mock or TCP, in the kernel-mode
//! driver it is SPMI through the `\Device\RESOURCE_HUB` device.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::arithmetic_side_effects))]

pub mod apsd;
pub mod clock;
pub mod driver;
pub mod error;
pub mod icl;
pub mod journal;
pub mod manifest;
pub mod policy;
pub mod regs;
pub mod transport;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use apsd::AdapterType;
pub use clock::{Clock, ManualClock};
pub use driver::{ChargePlan, Charger, ChargerConfig, Detection, Monitor, State, Stats};
pub use error::{ChargerError, TransportError, TransportErrorKind};
pub use icl::IclEncoding;
pub use journal::{Event, EventKind, Journal, Level, NullJournal};
#[cfg(feature = "std")]
pub use manifest::render_spec;
pub use manifest::{AdapterSpec, RegisterSpec, adapter_table};
pub use policy::{ChargePolicy, Qc2Voltage, Qc35Support, policy_for};
pub use regs::RegAddr;
pub use transport::ChargerTransport;
