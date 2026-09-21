//! `nabu-charger`: the CLI of the Xiaomi Pad 5 charging driver.
//!
//! Subcommands:
//!
//! | Command | What it does |
//! |---|---|
//! | `demo` | runs the scenarios on the mock transport and writes the journal |
//! | `detect` | runs one detection on the mock or on the real TCP transport |
//! | `sim` | starts the device simulator (a bench without hardware) |
//! | `verify` | self-check: policies, APSD decoding, current grid |
//!
//! Examples:
//!
//! ```text
//! cargo run -p cli -- demo
//! cargo run -p cli -- verify
//! cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
//! cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
//! ```

use charger_core::testkit::Fault;
use charger_core::{
    AdapterType, Charger, ChargerConfig, ChargerError, ChargerTransport, Clock, Event, EventKind,
    IclEncoding, Journal, Level, NullJournal, Qc35Support, TransportErrorKind, policy_for, regs,
};
use clap::{Parser, Subcommand, ValueEnum};
use host::journal::JsonlJournal;
use host::prelude::{
    MockTransport, RunOptions, SessionOutcome, Simulator, State, SystemClock, TcpTransport,
    run_until_ready,
};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Which transport to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TransportKind {
    /// In-memory mock transport.
    Mock,
    /// Real TCP transport.
    Tcp,
}

/// Which adapter is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AdapterKind {
    /// No power.
    Detached,
    /// Standard USB port.
    Sdp,
    /// BC1.2 charging port.
    Dcp,
    /// Quick Charge 2.0.
    Hvdcp2,
    /// Quick Charge 3.0.
    Hvdcp3,
    /// Quick Charge 3.5.
    Hvdcp3p5,
    /// Unknown detection pattern.
    Unknown,
}

impl AdapterKind {
    const fn to_adapter(self) -> Option<AdapterType> {
        match self {
            Self::Detached | Self::Unknown => None,
            Self::Sdp => Some(AdapterType::Sdp),
            Self::Dcp => Some(AdapterType::Dcp),
            Self::Hvdcp2 => Some(AdapterType::Hvdcp2),
            Self::Hvdcp3 => Some(AdapterType::Hvdcp3),
            Self::Hvdcp3p5 => Some(AdapterType::Hvdcp3P5),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Detached => "DETACHED",
            Self::Sdp => "SDP",
            Self::Dcp => "DCP",
            Self::Hvdcp2 => "HVDCP2",
            Self::Hvdcp3 => "HVDCP3",
            Self::Hvdcp3p5 => "HVDCP3P5",
            Self::Unknown => "UNKNOWN",
        }
    }

    const fn mock(self) -> MockBuilder {
        match self {
            Self::Detached => MockBuilder::Detached,
            Self::Sdp => MockBuilder::Adapter(AdapterType::Sdp),
            Self::Dcp => MockBuilder::Adapter(AdapterType::Dcp),
            Self::Hvdcp2 => MockBuilder::Adapter(AdapterType::Hvdcp2),
            Self::Hvdcp3 => MockBuilder::Adapter(AdapterType::Hvdcp3),
            Self::Hvdcp3p5 => MockBuilder::Adapter(AdapterType::Hvdcp3P5),
            Self::Unknown => MockBuilder::Unknown,
        }
    }
}

/// How to build the mock for the scenario.
#[derive(Debug, Clone, Copy)]
enum MockBuilder {
    Detached,
    Adapter(AdapterType),
    Unknown,
}

impl MockBuilder {
    fn build(self) -> MockTransport {
        match self {
            Self::Detached => MockTransport::detached(),
            Self::Adapter(value) => MockTransport::for_adapter(value),
            Self::Unknown => MockTransport::unknown_pattern(),
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "nabu-charger",
    version,
    about = "Xiaomi Pad 5 (nabu) charging driver: adapter detection and input current policy"
)]
struct Cli {
    /// Log verbosity on stderr: error, warn, info, debug, trace.
    #[arg(long, default_value = "info", global = true)]
    log: String,
    /// Where to write the JSON journal of operations.
    #[arg(long, global = true)]
    journal: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Runs all scenarios on the mock and prints a table of results.
    Demo,
    /// Runs one detection and prints the result.
    Detect {
        /// Transport type.
        #[arg(long, value_enum, default_value_t = TransportKind::Mock)]
        transport: TransportKind,
        /// Which adapter is connected (for the mock).
        #[arg(long, value_enum, default_value_t = AdapterKind::Hvdcp3)]
        adapter: AdapterKind,
        /// Device address for the TCP transport.
        #[arg(long, default_value = "127.0.0.1:9700")]
        addr: String,
        /// Detection wait timeout, ms.
        #[arg(long, default_value_t = 3_000)]
        timeout_ms: u64,
    },
    /// Starts the device simulator (a bench without hardware).
    Sim {
        /// Which adapter to emulate.
        #[arg(long, value_enum, default_value_t = AdapterKind::Hvdcp3)]
        adapter: AdapterKind,
        /// Listen address.
        #[arg(long, default_value = "127.0.0.1:9700")]
        listen: String,
    },
    /// Checks the LN8000 charge pump on the mock I²C bus.
    Pump {
        /// Settings profile
        #[arg(long, value_enum, default_value_t = PumpProfile::Qc35)]
        profile: PumpProfile,
    },
    /// Self-check of the tables, the math and the error paths.
    Verify,
}

/// Charge pump settings profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PumpProfile {
    /// Class B for Quick Charge 3.5 (13 V, 2.8 A).
    Qc35,
    /// Conservative mode: 6.5 V, 1 A.
    Conservative,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.log);

    let result = match &cli.command {
        Command::Demo => run_demo(cli.journal.as_ref()),
        Command::Detect {
            transport,
            adapter,
            addr,
            timeout_ms,
        } => run_detect(
            *transport,
            *adapter,
            addr,
            *timeout_ms,
            cli.journal.as_ref(),
        ),
        Command::Sim { adapter, listen } => run_sim(*adapter, listen),
        Command::Pump { profile } => run_pump(*profile),
        Command::Verify => run_verify(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn journal_path(given: Option<&PathBuf>) -> PathBuf {
    given
        .cloned()
        .unwrap_or_else(|| PathBuf::from("artifacts").join("journal-demo.jsonl"))
}

fn run_demo(journal_arg: Option<&PathBuf>) -> Result<(), String> {
    let path = journal_path(journal_arg);
    let journal =
        JsonlJournal::create(&path).map_err(|err| format!("failed to open the journal: {err}"))?;
    let clock = SystemClock::start();

    let scenarios = [
        (
            AdapterKind::Hvdcp3p5,
            "stock power brick: before authentication QC3.5 appears as HVDCP3",
        ),
        (AdapterKind::Hvdcp3, "Quick Charge 3.0"),
        (AdapterKind::Hvdcp2, "Quick Charge 2.0"),
        (AdapterKind::Dcp, "BC1.2 charging port"),
        (AdapterKind::Sdp, "standard USB port"),
        (AdapterKind::Detached, "no power: a timeout is expected"),
        (
            AdapterKind::Unknown,
            "unknown pattern: a failure is expected",
        ),
    ];

    println!(
        "{:<10} {:<6} {:<10} {:<7} {:<6} note",
        "scenario", "result", "adapter", "current,µA", "pump"
    );
    println!("{}", "-".repeat(78));

    let mut failures = Vec::new();
    for (kind, note) in scenarios {
        // Two scenarios must end with a typed error: this is a check of the
        // error paths, not a demo failure.
        let expected_code = match kind {
            AdapterKind::Detached => Some("detection_timeout"),
            AdapterKind::Unknown => Some("unknown_adapter_pattern"),
            _ => None,
        };
        match demo_one(kind, &journal, &clock) {
            Ok(line) => println!("{line}"),
            Err(err) if expected_code == Some(err.code()) => {
                println!(
                    "{:<10} {:<6} {:<10} {:<7} {:<6} error {} - {note}",
                    kind.name(),
                    "failure",
                    "-",
                    "-",
                    "-",
                    err.code()
                );
            }
            Err(err) => {
                println!(
                    "{:<10} {:<6} {:<10} {:<7} {:<6} {note}",
                    kind.name(),
                    "error",
                    "-",
                    "-",
                    "-"
                );
                failures.push(format!("{}: {err}", kind.name()));
            }
        }
    }

    emit(&journal, &clock, EventKind::Close { ok: true });
    let _ = journal.flush();
    println!();
    println!("journal: {}", journal.path().display());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("scenarios with errors: {}", failures.join("; ")))
    }
}

fn demo_one(
    kind: AdapterKind,
    journal: &JsonlJournal,
    clock: &SystemClock,
) -> Result<String, ChargerError> {
    let transport = kind.mock().build();
    let mut charger = Charger::open(transport, clock, journal, ChargerConfig::for_nabu())?;
    let outcome = run_until_ready(&mut charger, clock, RunOptions::default(), |_| Ok(()))?;
    Ok(format!(
        "{:<10} {:<6} {:<10} {:<7} {:<6} {}",
        kind.name(),
        "ok",
        outcome.adapter.label(),
        outcome.plan.applied_icl_ua,
        if outcome.plan.policy.pump_eligible {
            "yes"
        } else {
            "no"
        },
        outcome.plan.policy.rationale,
    ))
}

fn run_detect(
    transport: TransportKind,
    adapter: AdapterKind,
    addr: &str,
    timeout_ms: u64,
    journal_arg: Option<&PathBuf>,
) -> Result<(), String> {
    let clock = SystemClock::start();
    let config = ChargerConfig {
        detect_timeout_ms: timeout_ms,
        ..ChargerConfig::for_nabu()
    };

    match transport {
        TransportKind::Mock => {
            let path = journal_path(journal_arg);
            let journal = JsonlJournal::create(&path)
                .map_err(|err| format!("failed to open the journal: {err}"))?;
            let mock = adapter.mock().build();
            let mut charger =
                Charger::open(mock, &clock, &journal, config).map_err(|err| err.to_string())?;
            let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |value| {
                println!("adapter detected: {value}");
                Ok(())
            })
            .map_err(|err| err.to_string())?;
            let _ = journal.flush();
            print_outcome(&outcome, &charger);
            println!("journal: {}", journal.path().display());
            Ok(())
        }
        TransportKind::Tcp => {
            let transport = TcpTransport::connect(addr, Duration::from_millis(timeout_ms))
                .map_err(|err| err.to_string())?;
            let mut charger = Charger::open(transport, &clock, &NullJournal, config)
                .map_err(|err| err.to_string())?;
            let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |value| {
                println!("adapter detected: {value}");
                Ok(())
            })
            .map_err(|err| err.to_string())?;
            print_outcome(&outcome, &charger);
            Ok(())
        }
    }
}

fn print_outcome<T, C, J>(outcome: &SessionOutcome, charger: &Charger<'_, T, C, J>)
where
    T: ChargerTransport,
    C: Clock,
    J: Journal,
{
    println!("transport      : {}", charger.transport_name());
    println!("adapter        : {}", outcome.adapter.label());
    println!("current limit  : {} µA", outcome.plan.applied_icl_ua);
    println!("register code  : 0x{:02X}", outcome.plan.icl_raw);
    println!("rationale      : {}", outcome.plan.policy.rationale);
    println!(
        "charge pump    : {}",
        if outcome.plan.policy.pump_eligible {
            "allowed"
        } else {
            "not required"
        }
    );
    println!(
        "counters       : reads {}, writes {}, retries {}, resets {}, errors {}",
        outcome.stats.reads,
        outcome.stats.writes,
        outcome.stats.retries,
        outcome.stats.resets,
        outcome.stats.errors
    );
    println!(
        "state          : {}",
        match outcome.state {
            State::Ready => "ready",
            State::Closed => "closed",
            State::Detecting => "detecting",
            State::Idle => "idle",
            State::Faulted => "faulted",
        }
    );
}

fn run_sim(adapter: AdapterKind, listen: &str) -> Result<(), String> {
    let value = adapter.to_adapter().unwrap_or(AdapterType::Unknown);
    let sim = Simulator::start(value).map_err(|err| format!("simulator failed to start: {err}"))?;
    println!("simulator listening on {}", sim.addr());
    println!("requested address: {listen} (to change the address, run it from your own network)");
    println!(
        "connect with: nabu-charger detect --transport tcp --addr {}",
        sim.addr()
    );
    println!("Ctrl+C to stop");
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn run_pump(profile: PumpProfile) -> Result<(), String> {
    use ln8000::testkit::MockPumpBus;
    use ln8000::{AdcChannel, OpMode, Pump, PumpConfig};

    let config = match profile {
        PumpProfile::Qc35 => PumpConfig::for_qc35_class_b(),
        PumpProfile::Conservative => PumpConfig::conservative(),
    };

    let mut bus = MockPumpBus::new();
    // The mock models the chip: a write to SYS_CTRL changes SYS_STS.
    //
    // We show the three channels the driver uses for alarms
    // (IIN, VIN, VBAT): in the mock their byte pairs do not overlap. The other
    // channels share bytes with their neighbors, which is a property of the
    // hardware, so the demo does not print them to avoid meaningless numbers.
    bus.set_reg(AdcChannel::Iin.register(), 0x64); // pair (0x64, 0x00) -> 489 mA
    bus.set_reg(AdcChannel::Vin.register(), 0xC8); // pair (0xC8, 0x00) -> 3.2 V
    bus.set_reg(AdcChannel::Vbat.register(), 0x9C); // pair (0x9C, 0x02) -> 4.34 V
    bus.set_reg(AdcChannel::Vbat.register().saturating_add(1), 0x02);

    let mut pump = Pump::open(bus, config).map_err(|err| format!("open failed: {err}"))?;
    println!("bus           : {}", pump.bus_name());
    println!("state         : {}", pump.state().label());

    pump.configure()
        .map_err(|err| format!("configuration failed: {err}"))?;
    println!("after configuration: {}", pump.state().label());

    let mode = pump
        .enable_switching()
        .map_err(|err| format!("2:1 mode did not start: {err}"))?;
    println!("mode          : {} (code {})", mode.label(), mode.code());
    assert_eq!(mode, OpMode::Switching);

    let status = pump.status().map_err(|err| err.to_string())?;
    println!(
        "SYS_STS       : 0x{:02X} (current loop: {}, voltage loop: {})",
        status.sys_sts,
        if status.iin_loop_active() {
            "yes"
        } else {
            "no"
        },
        if status.vfloat_loop_active() {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "faults        : {}",
        if status.has_critical_fault() {
            status.fault_summary()
        } else {
            "none"
        }
    );

    println!("\nADC readings (mock), alarm channels:");
    for channel in [AdcChannel::Iin, AdcChannel::Vin, AdcChannel::Vbat] {
        let value = pump.read_adc(channel).map_err(|err| err.to_string())?;
        println!(
            "  {:<9} ADC{:<2} {:>9} {}",
            channel.label(),
            channel.adc_index(),
            value,
            channel.unit()
        );
    }

    let (writes, reads) = pump.counters();
    println!("\noperations    : writes {writes}, reads {reads}");
    pump.standby().map_err(|err| err.to_string())?;
    println!("after standby : {}", pump.op_mode().label());
    Ok(())
}

fn run_verify() -> Result<(), String> {
    println!("{}", charger_core::render_spec());

    let mut problems: Vec<String> = Vec::new();
    let qc35 = Qc35Support::default();
    let encoding = IclEncoding::default();

    // 1. Policy: the current matches the table from the reference driver.
    let expected = [
        (AdapterType::Sdp, 500_000_u32),
        (AdapterType::Dcp, 1_500_000),
        (AdapterType::Hvdcp2, 1_500_000),
        (AdapterType::Hvdcp3, 3_000_000),
        (AdapterType::Hvdcp3P5, 3_000_000),
    ];
    for (adapter, want) in expected {
        let got = policy_for(adapter, qc35).icl_ua;
        if got != want {
            problems.push(format!(
                "policy {}: expected {want} µA, got {got}",
                adapter.label()
            ));
        }
    }

    // 2. Current grid: encoding and decoding agree.
    for adapter in [
        AdapterType::Sdp,
        AdapterType::Dcp,
        AdapterType::Hvdcp2,
        AdapterType::Hvdcp3,
        AdapterType::Hvdcp3P5,
    ] {
        let target = encoding.quantize_down(policy_for(adapter, qc35).icl_ua);
        match encoding.encode(target) {
            Ok(raw) => {
                let back = encoding.decode(raw);
                if back != target {
                    problems.push(format!(
                        "{}: code 0x{raw:02X} gives {back} instead of {target}",
                        adapter.label()
                    ));
                }
            }
            Err(err) => problems.push(format!("{}: encoding error: {err}", adapter.label())),
        }
    }

    // 3. APSD decoding against the pattern table.
    for adapter in [
        AdapterType::Sdp,
        AdapterType::Cdp,
        AdapterType::Dcp,
        AdapterType::Hvdcp2,
        AdapterType::Hvdcp3,
    ] {
        let status = regs::APSD_DTC_STATUS_DONE
            | if adapter.is_hvdcp() {
                regs::QC_CHARGER
            } else {
                0
            };
        match AdapterType::decode(status, adapter.apsd_pattern(), qc35) {
            Ok(value) if value == adapter => {}
            Ok(value) => {
                problems.push(format!("{}: decoded as {}", adapter.label(), value.label()));
            }
            Err(err) => problems.push(format!("{}: decoding error: {err}", adapter.label())),
        }
    }

    // 4. Error path: a read failure must come back as a typed error.
    let clock = SystemClock::start();
    let journal = charger_core::testkit::VecJournal::new();
    let mut mock = MockTransport::hvdcp3();
    mock.push_fault(Fault::ReadError {
        addr: regs::APSD_STATUS,
        kind: TransportErrorKind::Timeout,
        times: 1,
    });
    let strict = ChargerConfig {
        max_transport_retries: 0,
        ..ChargerConfig::for_testing()
    };
    match Charger::open(mock, &clock, &journal, strict) {
        Ok(_) => problems.push("read failure during open was not detected".to_owned()),
        Err(err) if err.code() == "transport" => {}
        Err(err) => problems.push(format!("unexpected open error: {err}")),
    }

    if problems.is_empty() {
        println!("self-check: passed (policies, current grid, APSD decoding, error path)");
        Ok(())
    } else {
        for problem in &problems {
            println!("problem: {problem}");
        }
        Err(format!("self-check failed: {} findings", problems.len()))
    }
}

fn emit(journal: &JsonlJournal, clock: &SystemClock, kind: EventKind) {
    journal.event(&Event {
        seq: 0,
        ts_ms: clock.now_ms(),
        request_id: 0,
        level: Level::Info,
        kind,
    });
}
