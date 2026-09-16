//! `nabu-charger` — утилита драйвера зарядки Xiaomi Pad 5.
//!
//! Подкоманды:
//!
//! | Команда | Что делает |
//! |---|---|
//! | `demo` | прогоняет сценарии на мок-транспорте и пишет журнал |
//! | `detect` | прогоняет детекцию на моке или на реальном транспорте по TCP |
//! | `sim` | поднимает симулятор устройства (стенд без железа) |
//! | `verify` | самопроверка: политики, декодирование APSD, сетка тока |
//!
//! Примеры:
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

/// Каким транспортом пользоваться.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TransportKind {
    /// Мок-транспорт в памяти.
    Mock,
    /// Реальный TCP-транспорт.
    Tcp,
}

/// Какой адаптер подключён.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AdapterKind {
    /// Питания нет.
    Detached,
    /// Стандартный порт USB.
    Sdp,
    /// Порт зарядки BC1.2.
    Dcp,
    /// Quick Charge 2.0.
    Hvdcp2,
    /// Quick Charge 3.0.
    Hvdcp3,
    /// Quick Charge 3.5.
    Hvdcp3p5,
    /// Неизвестный образец детекции.
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

/// Как собрать мок для сценария.
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
    about = "Драйвер зарядки Xiaomi Pad 5 (nabu): детекция адаптера и политика входного тока"
)]
struct Cli {
    /// Подробность журнала в stderr: error, warn, info, debug, trace.
    #[arg(long, default_value = "info", global = true)]
    log: String,
    /// Куда писать JSON-журнал операций.
    #[arg(long, global = true)]
    journal: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Прогоняет все сценарии на моке и печатает таблицу результатов.
    Demo,
    /// Прогоняет одну детекцию и печатает результат.
    Detect {
        /// Тип транспорта.
        #[arg(long, value_enum, default_value_t = TransportKind::Mock)]
        transport: TransportKind,
        /// Какой адаптер подключён (для мока).
        #[arg(long, value_enum, default_value_t = AdapterKind::Hvdcp3)]
        adapter: AdapterKind,
        /// Адрес устройства для транспорта TCP.
        #[arg(long, default_value = "127.0.0.1:9700")]
        addr: String,
        /// Таймаут ожидания детекции, мс.
        #[arg(long, default_value_t = 3_000)]
        timeout_ms: u64,
    },
    /// Поднимает симулятор устройства (стенд без железа).
    Sim {
        /// Какой адаптер эмулировать.
        #[arg(long, value_enum, default_value_t = AdapterKind::Hvdcp3)]
        adapter: AdapterKind,
        /// Адрес прослушивания.
        #[arg(long, default_value = "127.0.0.1:9700")]
        listen: String,
    },
    /// Проверка charge pump LN8000 на мок-шине I²C.
    Pump {
        /// Профиль настроек
        #[arg(long, value_enum, default_value_t = PumpProfile::Qc35)]
        profile: PumpProfile,
    },
    /// Самопроверка таблиц, математики и ошибочных путей.
    Verify,
}

/// Профиль настроек charge pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PumpProfile {
    /// Класс B для Quick Charge 3.5 (13 В, 2.8 А).
    Qc35,
    /// Осторожный режим: 6.5 В, 1 А.
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
            eprintln!("ошибка: {message}");
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
        JsonlJournal::create(&path).map_err(|err| format!("не удалось открыть журнал: {err}"))?;
    let clock = SystemClock::start();

    let scenarios = [
        (
            AdapterKind::Hvdcp3p5,
            "родной блок: до аутентификации QC3.5 виден как HVDCP3",
        ),
        (AdapterKind::Hvdcp3, "Quick Charge 3.0"),
        (AdapterKind::Hvdcp2, "Quick Charge 2.0"),
        (AdapterKind::Dcp, "порт зарядки BC1.2"),
        (AdapterKind::Sdp, "стандартный порт USB"),
        (
            AdapterKind::Detached,
            "питание отсутствует: ожидается таймаут",
        ),
        (AdapterKind::Unknown, "неизвестный образец: ожидается отказ"),
    ];

    println!(
        "{:<10} {:<6} {:<10} {:<7} {:<6} пояснение",
        "сценарий", "итог", "адаптер", "ток,мкА", "pump"
    );
    println!("{}", "-".repeat(78));

    let mut failures = Vec::new();
    for (kind, note) in scenarios {
        // Два сценария обязаны закончиться типизированной ошибкой: это проверка
        // ошибочных путей, а не сбой демонстрации.
        let expected_code = match kind {
            AdapterKind::Detached => Some("detection_timeout"),
            AdapterKind::Unknown => Some("unknown_adapter_pattern"),
            _ => None,
        };
        match demo_one(kind, &journal, &clock) {
            Ok(line) => println!("{line}"),
            Err(err) if expected_code == Some(err.code()) => {
                println!(
                    "{:<10} {:<6} {:<10} {:<7} {:<6} ошибка {} — {note}",
                    kind.name(),
                    "отказ",
                    "—",
                    "—",
                    "—",
                    err.code()
                );
            }
            Err(err) => {
                println!(
                    "{:<10} {:<6} {:<10} {:<7} {:<6} {note}",
                    kind.name(),
                    "ошибка",
                    "—",
                    "—",
                    "—"
                );
                failures.push(format!("{}: {err}", kind.name()));
            }
        }
    }

    emit(&journal, &clock, EventKind::Close { ok: true });
    let _ = journal.flush();
    println!();
    println!("журнал: {}", journal.path().display());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("сценарии с ошибками: {}", failures.join("; ")))
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
        "ок",
        outcome.adapter.label(),
        outcome.plan.applied_icl_ua,
        if outcome.plan.policy.pump_eligible {
            "да"
        } else {
            "нет"
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
                .map_err(|err| format!("не удалось открыть журнал: {err}"))?;
            let mock = adapter.mock().build();
            let mut charger =
                Charger::open(mock, &clock, &journal, config).map_err(|err| err.to_string())?;
            let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |value| {
                println!("распознан адаптер: {value}");
                Ok(())
            })
            .map_err(|err| err.to_string())?;
            let _ = journal.flush();
            print_outcome(&outcome, &charger);
            println!("журнал: {}", journal.path().display());
            Ok(())
        }
        TransportKind::Tcp => {
            let transport = TcpTransport::connect(addr, Duration::from_millis(timeout_ms))
                .map_err(|err| err.to_string())?;
            let mut charger = Charger::open(transport, &clock, &NullJournal, config)
                .map_err(|err| err.to_string())?;
            let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |value| {
                println!("распознан адаптер: {value}");
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
    println!("транспорт      : {}", charger.transport_name());
    println!("адаптер        : {}", outcome.adapter.label());
    println!("лимит тока     : {} мкА", outcome.plan.applied_icl_ua);
    println!("код регистра   : 0x{:02X}", outcome.plan.icl_raw);
    println!("обоснование    : {}", outcome.plan.policy.rationale);
    println!(
        "charge pump    : {}",
        if outcome.plan.policy.pump_eligible {
            "разрешён"
        } else {
            "не требуется"
        }
    );
    println!(
        "счётчики       : чтений {}, записей {}, повторов {}, сбросов {}, ошибок {}",
        outcome.stats.reads,
        outcome.stats.writes,
        outcome.stats.retries,
        outcome.stats.resets,
        outcome.stats.errors
    );
    println!(
        "состояние      : {}",
        match outcome.state {
            State::Ready => "готов",
            State::Closed => "закрыт",
            State::Detecting => "идёт детекция",
            State::Idle => "ожидание",
            State::Faulted => "неисправность",
        }
    );
}

fn run_sim(adapter: AdapterKind, listen: &str) -> Result<(), String> {
    let value = adapter.to_adapter().unwrap_or(AdapterType::Unknown);
    let sim = Simulator::start(value).map_err(|err| format!("симулятор не запустился: {err}"))?;
    println!("симулятор слушает {}", sim.addr());
    println!("запрошенный адрес: {listen} (для смены адреса запускайте из своей сети)");
    println!(
        "подключение: nabu-charger detect --transport tcp --addr {}",
        sim.addr()
    );
    println!("Ctrl+C — остановка");
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
    // Мок моделирует чип: запись в SYS_CTRL меняет SYS_STS.
    //
    // Показываем три канала, которые драйвер использует для алармов
    // (IIN, VIN, VBAT): в моке их пары байт не пересекаются. Остальные каналы
    // делят байты с соседями — это свойство аппаратуры, поэтому в демо они не
    // выводятся, чтобы не показывать бессмысленные числа.
    bus.set_reg(AdcChannel::Iin.register(), 0x64); // пара (0x64, 0x00) → 489 мА
    bus.set_reg(AdcChannel::Vin.register(), 0xC8); // пара (0xC8, 0x00) → 3.2 В
    bus.set_reg(AdcChannel::Vbat.register(), 0x9C); // пара (0x9C, 0x02) → 4.34 В
    bus.set_reg(AdcChannel::Vbat.register() + 1, 0x02);

    let mut pump = Pump::open(bus, config).map_err(|err| format!("открытие не удалось: {err}"))?;
    println!("шина          : {}", pump.bus_name());
    println!("состояние     : {}", pump.state().label());

    pump.configure()
        .map_err(|err| format!("настройка не удалась: {err}"))?;
    println!("после настройки: {}", pump.state().label());

    let mode = pump
        .enable_switching()
        .map_err(|err| format!("режим 2:1 не включился: {err}"))?;
    println!("режим         : {} (код {})", mode.label(), mode.code());
    assert_eq!(mode, OpMode::Switching);

    let status = pump.status().map_err(|err| err.to_string())?;
    println!(
        "SYS_STS       : 0x{:02X} (петля тока: {}, петля напряжения: {})",
        status.sys_sts,
        if status.iin_loop_active() {
            "да"
        } else {
            "нет"
        },
        if status.vfloat_loop_active() {
            "да"
        } else {
            "нет"
        }
    );
    println!(
        "отказы        : {}",
        if status.has_critical_fault() {
            status.fault_summary()
        } else {
            "нет"
        }
    );

    println!("\nпоказания АЦП (мок), каналы алармов:");
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
    println!("\nопераций      : записей {writes}, чтений {reads}");
    pump.standby().map_err(|err| err.to_string())?;
    println!("после standby : {}", pump.op_mode().label());
    Ok(())
}

fn run_verify() -> Result<(), String> {
    println!("{}", charger_core::render_spec());

    let mut problems: Vec<String> = Vec::new();
    let qc35 = Qc35Support::default();
    let encoding = IclEncoding::default();

    // 1. Политика: ток соответствует таблице из эталонного драйвера.
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
                "политика {}: ожидалось {want} мкА, получено {got}",
                adapter.label()
            ));
        }
    }

    // 2. Сетка тока: кодирование и декодирование сходятся.
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
                        "{}: код 0x{raw:02X} даёт {back} вместо {target}",
                        adapter.label()
                    ));
                }
            }
            Err(err) => problems.push(format!("{}: ошибка кодирования: {err}", adapter.label())),
        }
    }

    // 3. Декодирование APSD по таблице образцов.
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
            Ok(value) => problems.push(format!(
                "{}: декодировано как {}",
                adapter.label(),
                value.label()
            )),
            Err(err) => problems.push(format!("{}: ошибка декодирования: {err}", adapter.label())),
        }
    }

    // 4. Ошибочный путь: сбой чтения должен вернуться типизированной ошибкой.
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
        Ok(_) => problems.push("сбой чтения при открытии не обнаружен".to_owned()),
        Err(err) if err.code() == "transport" => {}
        Err(err) => problems.push(format!("неожиданная ошибка открытия: {err}")),
    }

    if problems.is_empty() {
        println!(
            "самопроверка: пройдена (политики, сетка тока, декодирование APSD, ошибочный путь)"
        );
        Ok(())
    } else {
        for problem in &problems {
            println!("проблема: {problem}");
        }
        Err(format!(
            "самопроверка не пройдена: {} замечаний",
            problems.len()
        ))
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
