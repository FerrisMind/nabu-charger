//! Host layer of the `nabu` charging driver: transport, clock, journal, device simulator.
//!
//! The core ([`core`]) knows nothing about the operating system. Everything that
//! needs `std` (sockets, files, `tracing`, threads) lives here.
//!
//! # Quick start
//!
//! ```no_run
//! use host::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let clock = SystemClock::start();
//! let transport = MockTransport::hvdcp3();
//! let journal = JsonlJournal::create("artifacts/journal.jsonl")?;
//! let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::for_nabu())?;
//! let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))?;
//! println!("adapter: {} -> {} µA", outcome.adapter, outcome.plan.applied_icl_ua);
//! # Ok(())
//! # }
//! ```
//!
//! # Layout
//!
//! | Module | Purpose |
//! |---|---|
//! | [`clock`] | monotonic process clock |
//! | [`mock`] | mock transport with a transaction log (no hardware) |
//! | [`tcp`] | real TCP transport to a bench or simulator |
//! | [`sim`] | device simulator: SMB registers and the transport protocol |
//! | [`journal`] | journal recording to JSON Lines and a bridge into `tracing` |
//! | [`runner`] | blocking session loop for tools and tests |

pub mod clock;
pub mod journal;
pub mod mock;
pub mod runner;
pub mod sim;
pub mod tcp;

/// Commonly used types.
pub mod prelude {
    pub use crate::clock::SystemClock;
    pub use crate::journal::{Fanout, JsonlJournal, TracingJournal};
    pub use crate::mock::MockTransport;
    pub use crate::runner::{RunOptions, SessionOutcome, run_until_ready};
    pub use crate::sim::{Simulator, SimulatorHandle};
    pub use crate::tcp::TcpTransport;
    pub use charger_core::{
        AdapterType, ChargePlan, Charger, ChargerConfig, ChargerError, ChargerTransport, Clock,
        Detection, Journal, Monitor, NullJournal, State, Stats,
    };
}
