//! Минимальный пример: детекция адаптера на мок-транспорте.
//!
//! Запуск:
//!
//! ```text
//! cargo run -p host --example minimal
//! ```
//!
//! Ожидаемый вывод:
//!
//! ```text
//! адаптер: HVDCP3
//! лимит входного тока: 3000000 мкА (код 0x1D)
//! ```

use charger_core::{ChargerConfig, ChargerError};
use host::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let clock = SystemClock::start();
    let transport = MockTransport::hvdcp3();
    let journal = TracingJournal;

    let mut charger =
        Charger::open(transport, &clock, &journal, ChargerConfig::for_nabu()).map_err(describe)?;

    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |adapter| {
        println!("распознан адаптер: {adapter}");
        Ok(())
    })
    .map_err(describe)?;

    println!("адаптер: {}", outcome.adapter.label());
    println!(
        "лимит входного тока: {} мкА (код 0x{:02X})",
        outcome.plan.applied_icl_ua, outcome.plan.icl_raw
    );
    println!("обоснование: {}", outcome.plan.policy.rationale);
    charger.close();
    Ok(())
}

fn describe(error: ChargerError) -> String {
    format!("{error} [{}]", error.code())
}
