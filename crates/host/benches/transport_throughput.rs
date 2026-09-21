//! Transport measurements: how many register accesses the channel sustains.
//!
//! Two transports are compared: the in-memory mock (upper bound) and real TCP to
//! the device simulator (lower bound for the network).
//!
//! ```text
//! cargo bench -p host --bench transport_throughput
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use charger_core::{AdapterType, ChargerTransport, regs};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use host::mock::MockTransport;
use host::sim::Simulator;
use host::tcp::TcpTransport;
use std::hint::black_box;
use std::time::Duration;

const OPS: u64 = 512;

fn bench_mock(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/mock");
    group.throughput(Throughput::Elements(OPS));
    group.bench_function("read", |b| {
        b.iter(|| {
            let mut transport = MockTransport::hvdcp3();
            for _ in 0..OPS {
                if let Ok(value) = transport.read(regs::APSD_STATUS) {
                    black_box(value);
                }
            }
        });
    });
    group.bench_function("write", |b| {
        b.iter(|| {
            let mut transport = MockTransport::hvdcp3();
            for index in 0..OPS {
                let value = u8::try_from(index % 32).unwrap_or(0);
                if transport
                    .write(regs::USBIN_CURRENT_LIMIT_CFG, value)
                    .is_ok()
                {
                    black_box(value);
                }
            }
        });
    });
    group.finish();
}

fn bench_tcp(c: &mut Criterion) {
    let Ok(sim) = Simulator::start(AdapterType::Hvdcp3) else {
        return;
    };
    let mut group = c.benchmark_group("transport/tcp_simulator");
    // The network is slower than memory: reduce the volume so a run is not slow.
    group.throughput(Throughput::Elements(64));
    group.sample_size(30);
    group.bench_function("read", |b| {
        b.iter(|| {
            let Ok(mut transport) = TcpTransport::connect(sim.addr(), Duration::from_millis(500))
            else {
                return;
            };
            for _ in 0..64 {
                if let Ok(value) = transport.read(regs::APSD_STATUS) {
                    black_box(value);
                }
            }
        });
    });
    group.finish();
    sim.stop();
}

criterion_group!(benches, bench_mock, bench_tcp);
criterion_main!(benches);
