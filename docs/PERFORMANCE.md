# Baseline performance

Measured with `criterion` on this machine (AMD Ryzen 7 3700X, Windows 11,
Rust 1.97.0, `bench` profile). Serves as the baseline for comparing future versions.

Commands:

```powershell
cargo bench -p host --bench core_state_machine  -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 20 --noplot
cargo bench -p host --bench transport_throughput -- --warm-up-time 0.5 --measurement-time 1.5 --sample-size 15 --noplot
```

Raw outputs: `artifacts/bench-core.txt`, `artifacts/bench-transport.txt`.

## Core logic

| Scenario | Time | What it means |
|---|---|---|
| `session/open` | 232 ns | check of communication with the peripheral (reset + read) |
| `session/detect_hvdcp3` | 625 ns | one detection step with a ready APSD |
| `session/full/sdp` | 836 ns | full cycle: open -> detection -> policy |
| `session/full/dcp` | 791 ns | the same for a charging port |
| `session/full/hvdcp2` | 931 ns | the same for QC2 |
| `session/full/hvdcp3` | 902 ns | the same for QC3 |
| `session/full/hvdcp3p5` | 864 ns | the same for QC3.5 |
| `decode/apsd_result` | 305 ps | parsing of one detection result |

A full session cycle fits into **~0.9 us** with no hardware accesses: this is pure
logic work (parsing, policy, journal, checks). In the kernel driver the SPMI
transaction time is added to this, which is determined by the bus, not by our code.

## Transport

| Transport, operation | Time per 512 operations | Throughput |
|---|---|---|
| mock, read | 3.15 us | 163 million operations/s |
| mock, write | 3.20 us | 160 million operations/s |
| TCP (localhost, simulator), read of 64 operations | 8.38 ms | 7 640 operations/s |

The mock shows the upper bound of useful work; TCP to the simulator is the lower
bound for the network (~131 us per round-trip). The real SPMI bus is expected in the
range between them: one SPMI transaction is a few microseconds.

## How to compare with future versions

1. Run the same commands.
2. Compare with the tables above: a regression is an increase of more than 20% on
   any scenario with unchanged hardware and toolchain.
3. If the regression is confirmed, look at `target/criterion/*/report/index.html`.

Measurements are not part of CI: they depend on the machine. CI only checks that the
benchmarks build and run.
