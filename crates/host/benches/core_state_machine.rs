//! Замеры ядра драйвера: открытие, детекция, применение политики, полная сессия.
//!
//! Запуск:
//!
//! ```text
//! cargo bench -p host --bench core_state_machine
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use charger_core::testkit::VecJournal;
use charger_core::{AdapterType, Charger, ChargerConfig, ManualClock, NullJournal, Qc35Support};
use criterion::{Criterion, criterion_group, criterion_main};
use host::mock::MockTransport;
use host::prelude::RunOptions;
use host::runner::run_until_ready;
use std::hint::black_box;

fn bench_open(c: &mut Criterion) {
    c.bench_function("session/open", |b| {
        b.iter(|| {
            let clock = ManualClock::new();
            let Ok(charger) = Charger::open(
                MockTransport::hvdcp3(),
                &clock,
                &NullJournal,
                ChargerConfig::for_testing(),
            ) else {
                return;
            };
            black_box(charger.state());
        });
    });
}

fn bench_detect(c: &mut Criterion) {
    c.bench_function("session/detect_hvdcp3", |b| {
        b.iter(|| {
            let clock = ManualClock::new();
            let journal = VecJournal::new();
            let Ok(mut charger) = Charger::open(
                MockTransport::hvdcp3(),
                &clock,
                &journal,
                ChargerConfig::for_testing(),
            ) else {
                return;
            };
            clock.advance_ms(10);
            if let Ok(ready) = charger.detect_step() {
                black_box(ready);
            }
        });
    });
}

fn bench_full_session(c: &mut Criterion) {
    let mut group = c.benchmark_group("session/full");
    for (name, adapter) in [
        ("sdp", AdapterType::Sdp),
        ("dcp", AdapterType::Dcp),
        ("hvdcp2", AdapterType::Hvdcp2),
        ("hvdcp3", AdapterType::Hvdcp3),
        ("hvdcp3p5", AdapterType::Hvdcp3P5),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let clock = ManualClock::new();
                let journal = VecJournal::new();
                let Ok(mut charger) = Charger::open(
                    MockTransport::for_adapter(adapter),
                    &clock,
                    &journal,
                    ChargerConfig::for_testing(),
                ) else {
                    return;
                };
                if let Ok(outcome) =
                    run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))
                {
                    black_box(outcome.plan.applied_icl_ua);
                }
            });
        });
    }
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    c.bench_function("decode/apsd_result", |b| {
        b.iter(|| {
            let status = charger_core::regs::APSD_DTC_STATUS_DONE | charger_core::regs::QC_CHARGER;
            let result = AdapterType::Hvdcp3.apsd_pattern();
            if let Ok(adapter) = AdapterType::decode(status, result, Qc35Support::default()) {
                black_box(adapter);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_open,
    bench_detect,
    bench_full_session,
    bench_decode
);
criterion_main!(benches);
