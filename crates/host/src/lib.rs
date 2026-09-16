//! Хост-слой драйвера зарядки `nabu`: транспорт, часы, журнал, симулятор устройства.
//!
//! Ядро ([`core`]) не знает про операционную систему. Всё, что
//! требует `std` — сокеты, файлы, `tracing`, потоки — живёт здесь.
//!
//! # Быстрый старт
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
//! println!("адаптер: {} → {} мкА", outcome.adapter, outcome.plan.applied_icl_ua);
//! # Ok(())
//! # }
//! ```
//!
//! # Состав
//!
//! | Модуль | Назначение |
//! |---|---|
//! | [`clock`] | монотонные часы процесса |
//! | [`mock`] | мок-транспорт с журналом транзакций (без железа) |
//! | [`tcp`] | реальный транспорт по TCP к стенду или симулятору |
//! | [`sim`] | симулятор устройства: регистры SMB и протокол транспорта |
//! | [`journal`] | запись журнала в JSON Lines и мост в `tracing` |
//! | [`runner`] | блокирующий цикл сессии для утилит и тестов |

pub mod clock;
pub mod journal;
pub mod mock;
pub mod runner;
pub mod sim;
pub mod tcp;

/// Часто используемые типы.
pub mod prelude {
    pub use crate::clock::SystemClock;
    pub use crate::journal::{Fanout, JsonlJournal, TracingJournal};
    pub use crate::mock::MockTransport;
    pub use crate::runner::{RunOptions, SessionOutcome, run_until_ready};
    pub use crate::sim::{Simulator, SimulatorHandle};
    pub use crate::tcp::TcpTransport;
    pub use core::{
        AdapterType, ChargePlan, Charger, ChargerConfig, ChargerError, ChargerTransport, Clock,
        Detection, Journal, Monitor, NullJournal, State, Stats,
    };
}
