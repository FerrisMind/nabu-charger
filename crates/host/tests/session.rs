//! Интеграционные тесты хост-слоя: реальные сценарии поверх трейта транспорта.
//!
//! Здесь нет реального железа: логика драйвера проверяется на моке, а транспорт —
//! по TCP к симулятору устройства в этом же процессе.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use charger_core::testkit::Fault;
use charger_core::{
    AdapterType, Charger, ChargerConfig, ChargerError, Clock, Monitor, NullJournal,
    TransportErrorKind,
};
use host::journal::JsonlJournal;
use host::mock::MockTransport;
use host::prelude::*;
use host::tcp::TcpTransport;
use std::time::{Duration, Instant};

fn wait_for<F: FnMut() -> bool>(mut condition: F, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn session_on_mock_applies_policy_and_writes_journal() {
    let dir = tempfile::tempdir().expect("временный каталог");
    let path = dir.path().join("journal.jsonl");
    let journal = JsonlJournal::create(&path).expect("журнал создаётся");
    let clock = SystemClock::start();

    let transport = MockTransport::hvdcp3();
    let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::for_nabu())
        .expect("сессия открывается");
    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
        .expect("сессия проходит");

    assert_eq!(outcome.adapter, AdapterType::Hvdcp3);
    assert_eq!(outcome.plan.applied_icl_ua, 3_000_000);
    assert!(outcome.plan.policy.pump_eligible);
    charger.close();
    journal.flush().expect("журнал сброшен на диск");

    let text = std::fs::read_to_string(&path).expect("журнал читается");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() > 5, "в журнале должно быть несколько записей");

    for line in &lines {
        let value: serde_json::Value = serde_json::from_str(line).expect("строка — валидный JSON");
        assert!(value.get("seq").is_some(), "есть порядковый номер");
        assert!(value.get("ts_ms").is_some(), "есть метка времени");
        assert!(
            value.get("request_id").is_some(),
            "есть идентификатор запроса"
        );
        assert!(value.get("level").is_some(), "есть уровень");
        assert!(value.get("kind").is_some(), "есть тип события");
    }

    let detect = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("kind").and_then(|k| k.as_str()) == Some("detect"))
        .expect("есть запись о детекции");
    assert_eq!(detect["adapter"], "HVDCP3");
    assert_eq!(detect["raw_result"], serde_json::json!(0x48));
}

#[test]
fn session_over_tcp_against_simulator() {
    eprintln!("шаг 1: запуск симулятора");
    let sim = Simulator::start(AdapterType::Hvdcp3P5).expect("симулятор поднимается");
    eprintln!("шаг 2: адрес {}", sim.addr());
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("подключение к симулятору");
    eprintln!("шаг 3: подключились");

    // QC3.5 поднимается до HVDCP3P5 только после аутентификации: без неё
    // аппаратура сообщает образец HVDCP3, и драйвер честно говорит HVDCP3.
    let config = ChargerConfig {
        qc35: charger_core::Qc35Support::Supported {
            authenticated: true,
        },
        ..ChargerConfig::for_nabu()
    };
    let mut charger =
        Charger::open(transport, &clock, &NullJournal, config).expect("сессия открывается по сети");
    eprintln!("шаг 4: сессия открыта");
    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
        .expect("сессия проходит по сети");
    eprintln!("шаг 5: сессия прошла");

    assert_eq!(outcome.adapter, AdapterType::Hvdcp3P5);
    assert_eq!(charger.transport_name(), "tcp");
    assert_eq!(charger.state(), State::Ready);
    eprintln!("шаг 6: остановка симулятора");
    sim.stop();
    eprintln!("шаг 7: готово");
}

#[test]
fn adapter_change_is_detected_and_reapplied() {
    let sim = Simulator::start(AdapterType::Dcp).expect("симулятор поднимается");
    let handle = sim.handle();
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("подключение к симулятору");
    let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())
        .expect("сессия открывается");
    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
        .expect("первичная детекция");
    assert_eq!(outcome.adapter, AdapterType::Dcp);

    handle.set_adapter(AdapterType::Hvdcp3);
    let switched = wait_for(
        || match charger.monitor() {
            Ok(Monitor::Changed(adapter)) => adapter == AdapterType::Hvdcp3,
            _ => false,
        },
        Duration::from_secs(2),
    );
    assert!(switched, "смена адаптера должна обнаруживаться");
    let plan = charger
        .apply(AdapterType::Hvdcp3)
        .expect("политика переприменяется");
    assert_eq!(plan.applied_icl_ua, 3_000_000);
    sim.stop();
}

#[test]
fn power_removal_is_reported_as_detached() {
    let sim = Simulator::start(AdapterType::Hvdcp3).expect("симулятор поднимается");
    let handle = sim.handle();
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("подключение к симулятору");
    let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())
        .expect("сессия открывается");
    run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(())).expect("детекция");

    // Пропажа питания выражается снятым битом готовности в APSD_STATUS.
    handle.set_reg(charger_core::regs::APSD_STATUS, 0);
    handle.set_reg(charger_core::regs::APSD_RESULT_STATUS, 0);
    let detached = wait_for(
        || matches!(charger.monitor(), Ok(Monitor::Detached)),
        Duration::from_secs(2),
    );
    assert!(detached, "пропажа питания должна обнаруживаться");
    sim.stop();
}

#[test]
fn transport_faults_are_retried_and_surface_as_typed_errors() {
    let clock = SystemClock::start();
    let mut mock = MockTransport::hvdcp3();
    mock.push_fault(Fault::ReadError {
        addr: charger_core::regs::APSD_STATUS,
        kind: TransportErrorKind::Timeout,
        times: 1,
    });
    let charger = Charger::open(mock, &clock, &NullJournal, ChargerConfig::for_testing())
        .expect("один сбой переживается повтором");
    assert_eq!(charger.stats().retries, 1);
    assert_eq!(charger.stats().resets, 1);

    // Второй сбой исчерпывает бюджет повторов и превращается в типизированную ошибку.
    let mut strict = MockTransport::hvdcp3();
    strict.push_fault(Fault::ReadError {
        addr: charger_core::regs::APSD_STATUS,
        kind: TransportErrorKind::Disconnected,
        times: 4,
    });
    let config = ChargerConfig {
        max_transport_retries: 1,
        ..ChargerConfig::for_testing()
    };
    let result = Charger::open(strict, &clock, &NullJournal, config);
    match result {
        Ok(_) => panic!("сессия не должна открыться на мёртвом канале"),
        Err(err) => {
            assert!(matches!(err, ChargerError::Transport(_)));
            assert_eq!(err.code(), "transport");
            assert!(err.is_recoverable());
        }
    }
}

#[test]
fn failed_reset_blocks_opening() {
    let clock = SystemClock::start();
    let mut mock = MockTransport::hvdcp3();
    mock.fail_next_resets(1);
    mock.push_fault(Fault::ReadError {
        addr: charger_core::regs::APSD_STATUS,
        kind: TransportErrorKind::Io,
        times: 2,
    });
    let config = ChargerConfig {
        max_transport_retries: 1,
        ..ChargerConfig::for_testing()
    };
    let result = Charger::open(mock, &clock, &NullJournal, config);
    assert!(result.is_err(), "без сброса канала сессия не открывается");
}

#[test]
fn reopening_works_without_restarting_the_process() {
    let clock = SystemClock::start();
    for _ in 0..3 {
        let mut charger = Charger::open(
            MockTransport::hvdcp2(),
            &clock,
            &NullJournal,
            ChargerConfig::for_testing(),
        )
        .expect("повторное открытие");
        let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
            .expect("повторная сессия");
        assert_eq!(outcome.adapter, AdapterType::Hvdcp2);
        charger.close();
    }
}

#[test]
fn monitor_cycle_reapplies_policy_only_on_change() {
    let sim = Simulator::start(AdapterType::Sdp).expect("симулятор поднимается");
    let handle = sim.handle();
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("подключение к симулятору");
    let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())
        .expect("сессия открывается");
    run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(())).expect("детекция");

    assert!(
        host::runner::monitor_cycle(&mut charger)
            .expect("опрос")
            .is_none(),
        "без смены адаптера политика не переприменяется"
    );

    handle.set_adapter(AdapterType::Hvdcp2);
    let changed = wait_for(
        || matches!(host::runner::monitor_cycle(&mut charger), Ok(Some(_))),
        Duration::from_secs(2),
    );
    assert!(changed, "новая политика применяется после смены адаптера");
    assert_eq!(charger.adapter(), Some(AdapterType::Hvdcp2));
    sim.stop();
}

#[test]
fn journal_records_request_ids_and_monotonic_time() {
    let dir = tempfile::tempdir().expect("временный каталог");
    let path = dir.path().join("monotonic.jsonl");
    let journal = JsonlJournal::create(&path).expect("журнал создаётся");
    let clock = SystemClock::start();
    let mut charger = Charger::open(
        MockTransport::dcp(),
        &clock,
        &journal,
        ChargerConfig::for_testing(),
    )
    .expect("сессия открывается");
    let _ = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()));
    charger.close();
    journal.flush().expect("сброс журнала");

    let text = std::fs::read_to_string(&path).expect("журнал читается");
    let mut previous_ts = 0_u64;
    let mut previous_seq = 0_u64;
    let mut requests = 0_u64;
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("JSON");
        let ts = value["ts_ms"].as_u64().unwrap_or_default();
        let seq = value["seq"].as_u64().unwrap_or_default();
        assert!(ts >= previous_ts, "время не должно идти назад");
        assert!(seq > previous_seq, "номера записей возрастают");
        previous_ts = ts;
        previous_seq = seq;
        if value["request_id"].as_u64().unwrap_or_default() > 0 {
            requests += 1;
        }
    }
    assert!(requests > 3, "записи несут идентификатор запроса");
    assert!(clock.now_ms() < 60_000, "тест не должен занимать минуту");
}
