//! Host layer integration tests: real scenarios on top of the transport trait.
//!
//! There is no real hardware here: the driver logic is checked on the mock and the
//! transport over TCP against the device simulator in the same process.

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
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("journal.jsonl");
    let journal = JsonlJournal::create(&path).expect("the journal is created");
    let clock = SystemClock::start();

    let transport = MockTransport::hvdcp3();
    let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::for_nabu())
        .expect("the session opens");
    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
        .expect("the session completes");

    assert_eq!(outcome.adapter, AdapterType::Hvdcp3);
    assert_eq!(outcome.plan.applied_icl_ua, 3_000_000);
    assert!(outcome.plan.policy.pump_eligible);
    charger.close();
    journal.flush().expect("the journal is flushed to disk");

    let text = std::fs::read_to_string(&path).expect("the journal is readable");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() > 5, "the journal must contain several records");

    for line in &lines {
        let value: serde_json::Value = serde_json::from_str(line).expect("the line is valid JSON");
        assert!(value.get("seq").is_some(), "it has a sequence number");
        assert!(value.get("ts_ms").is_some(), "it has a timestamp");
        assert!(
            value.get("request_id").is_some(),
            "it has a request identifier"
        );
        assert!(value.get("level").is_some(), "it has a level");
        assert!(value.get("kind").is_some(), "it has an event kind");
    }

    let detect = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("kind").and_then(|k| k.as_str()) == Some("detect"))
        .expect("there is a detection record");
    assert_eq!(detect["adapter"], "HVDCP3");
    assert_eq!(detect["raw_result"], serde_json::json!(0x48));
}

#[test]
fn session_over_tcp_against_simulator() {
    eprintln!("step 1: starting the simulator");
    let sim = Simulator::start(AdapterType::Hvdcp3P5).expect("the simulator starts");
    eprintln!("step 2: address {}", sim.addr());
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("connection to the simulator");
    eprintln!("step 3: connected");

    // QC3.5 comes up as HVDCP3P5 only after authentication: without it the
    // hardware reports the HVDCP3 pattern, and the driver honestly says HVDCP3.
    let config = ChargerConfig {
        qc35: charger_core::Qc35Support::Supported {
            authenticated: true,
        },
        ..ChargerConfig::for_nabu()
    };
    let mut charger = Charger::open(transport, &clock, &NullJournal, config)
        .expect("the session opens over the network");
    eprintln!("step 4: session open");
    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
        .expect("the session completes over the network");
    eprintln!("step 5: session completed");

    assert_eq!(outcome.adapter, AdapterType::Hvdcp3P5);
    assert_eq!(charger.transport_name(), "tcp");
    assert_eq!(charger.state(), State::Ready);
    eprintln!("step 6: stopping the simulator");
    sim.stop();
    eprintln!("step 7: done");
}

#[test]
fn adapter_change_is_detected_and_reapplied() {
    let sim = Simulator::start(AdapterType::Dcp).expect("the simulator starts");
    let handle = sim.handle();
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("connection to the simulator");
    let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())
        .expect("the session opens");
    let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
        .expect("initial detection");
    assert_eq!(outcome.adapter, AdapterType::Dcp);

    handle.set_adapter(AdapterType::Hvdcp3);
    let switched = wait_for(
        || match charger.monitor() {
            Ok(Monitor::Changed(adapter)) => adapter == AdapterType::Hvdcp3,
            _ => false,
        },
        Duration::from_secs(2),
    );
    assert!(switched, "an adapter change must be detected");
    let plan = charger
        .apply(AdapterType::Hvdcp3)
        .expect("the policy is reapplied");
    assert_eq!(plan.applied_icl_ua, 3_000_000);
    sim.stop();
}

#[test]
fn power_removal_is_reported_as_detached() {
    let sim = Simulator::start(AdapterType::Hvdcp3).expect("the simulator starts");
    let handle = sim.handle();
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("connection to the simulator");
    let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())
        .expect("the session opens");
    run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(())).expect("detection");

    // Power removal is expressed by the cleared ready bit in APSD_STATUS.
    handle.set_reg(charger_core::regs::APSD_STATUS, 0);
    handle.set_reg(charger_core::regs::APSD_RESULT_STATUS, 0);
    let detached = wait_for(
        || matches!(charger.monitor(), Ok(Monitor::Detached)),
        Duration::from_secs(2),
    );
    assert!(detached, "power removal must be detected");
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
        .expect("a single fault is survived by a retry");
    assert_eq!(charger.stats().retries, 1);
    assert_eq!(charger.stats().resets, 1);

    // The second fault exhausts the retry budget and turns into a typed error.
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
        Ok(_) => panic!("the session must not open on a dead channel"),
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
    assert!(
        result.is_err(),
        "without a channel reset the session does not open"
    );
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
        .expect("reopening");
        let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
            .expect("a repeat session");
        assert_eq!(outcome.adapter, AdapterType::Hvdcp2);
        charger.close();
    }
}

#[test]
fn monitor_cycle_reapplies_policy_only_on_change() {
    let sim = Simulator::start(AdapterType::Sdp).expect("the simulator starts");
    let handle = sim.handle();
    let clock = SystemClock::start();
    let transport = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
        .expect("connection to the simulator");
    let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())
        .expect("the session opens");
    run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(())).expect("detection");

    assert!(
        host::runner::monitor_cycle(&mut charger)
            .expect("poll")
            .is_none(),
        "without an adapter change the policy is not reapplied"
    );

    handle.set_adapter(AdapterType::Hvdcp2);
    let changed = wait_for(
        || matches!(host::runner::monitor_cycle(&mut charger), Ok(Some(_))),
        Duration::from_secs(2),
    );
    assert!(
        changed,
        "the new policy is applied after the adapter change"
    );
    assert_eq!(charger.adapter(), Some(AdapterType::Hvdcp2));
    sim.stop();
}

#[test]
fn journal_records_request_ids_and_monotonic_time() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("monotonic.jsonl");
    let journal = JsonlJournal::create(&path).expect("the journal is created");
    let clock = SystemClock::start();
    let mut charger = Charger::open(
        MockTransport::dcp(),
        &clock,
        &journal,
        ChargerConfig::for_testing(),
    )
    .expect("the session opens");
    let _ = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()));
    charger.close();
    journal.flush().expect("journal flush");

    let text = std::fs::read_to_string(&path).expect("the journal is readable");
    let mut previous_ts = 0_u64;
    let mut previous_seq = 0_u64;
    let mut requests = 0_u64;
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("JSON");
        let ts = value["ts_ms"].as_u64().unwrap_or_default();
        let seq = value["seq"].as_u64().unwrap_or_default();
        assert!(ts >= previous_ts, "time must not go backwards");
        assert!(seq > previous_seq, "record numbers increase");
        previous_ts = ts;
        previous_seq = seq;
        if value["request_id"].as_u64().unwrap_or_default() > 0 {
            requests += 1;
        }
    }
    assert!(requests > 3, "records carry a request identifier");
    assert!(clock.now_ms() < 60_000, "the test must not take a minute");
}
