//! Minimal example: adapter detection on the mock transport.
//!
//! Run with:
//!
//! ```text
//! cargo run -p host --example minimal
//! ```
//!
//! Expected output:
//!
//! ```text
//! adapter: HVDCP3
//! input current limit: 3000000 µA (code 0x1D)
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
        println!("adapter detected: {adapter}");
        Ok(())
    })
    .map_err(describe)?;

    println!("adapter: {}", outcome.adapter.label());
    println!(
        "input current limit: {} µA (code 0x{:02X})",
        outcome.plan.applied_icl_ua, outcome.plan.icl_raw
    );
    println!("rationale: {}", outcome.plan.policy.rationale);
    charger.close();
    Ok(())
}

fn describe(error: ChargerError) -> String {
    format!("{error} [{}]", error.code())
}
