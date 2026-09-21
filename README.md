# nabu-fastcharge

**English** | [Русский](README.ru.md) | [Português (Brasil)](README.pt-BR.md)

Charging driver for the **Xiaomi Pad 5 (nabu, Snapdragon 860) on Windows on
ARM64**.

On Windows the tablet does not charge from any power supply - neither USB-A
(Quick Charge) nor USB-C (Power Delivery). The cause is not a missing fast-charge
feature: **the PMIC performs hardware adapter detection (APSD), but no Windows
component reads its result**, so the input current limit is never raised and the
charge never starts.

This driver closes exactly that gap: it reads the detection result and applies the
current policy.

## Layout

| Crate | What it is | Checks |
|---|---|---|
| [`crates/core`](crates/core) | SMB logic core: APSD detection, current policy, states, timeouts, journal. No `std`, no `unsafe` | 37 unit tests |
| [`crates/ln8000`](crates/ln8000) | LN8000 charge pump core (I²C 0x51): registers, modes, protections, ADC, session telemetry, thermal guard. No `std`, no `unsafe` | 125 unit tests, 6 integration tests, 2 doctests |
| [`crates/spb`](crates/spb) | SPB types and transfer-list building shared by the kernel drivers, declared by hand because `wdk-sys` does not generate them | 10 unit tests |
| [`crates/ln8000-kmdf`](crates/ln8000-kmdf) | LN8000 KMDF driver on the ACPI node `PEIC` over I²C (SPB / Resource Hub), plus install and diagnostics scripts | builds for ARM64 |
| [`crates/host`](crates/host) | Host layer: transports (mock, TCP), JSONL journal, `tracing`, device simulator, benchmarks | 9 integration tests, 2 doctests |
| [`crates/cli`](crates/cli) | The `nabu-charger` tool: `demo`, `detect`, `sim`, `pump`, `verify` | 4 CLI tests |
| [`crates/kmdf`](crates/kmdf) | Kernel-mode driver (KMDF) for ARM64 over the SPMI bus | built: `kmdf.sys` ARM64, signed, `infverif` passed |

`cargo test --workspace` covers 195 tests - unit, integration and doctests - in
the five crates of the root workspace. The two kernel-mode drivers declare their
own workspace, because they need the WDK and `cargo-wdk`, which the regular CI
environment does not have.

## Quick start

```powershell
# 1. The same checks CI runs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 2. Demonstration: every adapter type on the mock transport, plus a journal
cargo run -p cli -- --journal artifacts/journal-demo.jsonl demo

# 3. The LN8000 charge pump on a mock I²C bus (configure -> 2:1 mode -> status -> ADC)
cargo run -p cli -- pump --profile qc35

# 4. Self-check of the driver's tables and arithmetic
cargo run -p cli -- verify

# 5. A bench without hardware: the device simulator plus the real TCP transport
cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
```

`pump` on the mock bus. The mock is deterministic, so this output reproduces
exactly:

```text
bus           : mock
state         : probed
after configuration: configured
mode          : SWITCHING (code 3)
SYS_STS       : 0x04 (current loop: no, voltage loop: no)
faults        : none

ADC readings (mock), alarm channels:
  iin       ADC1     489000 uA
  vin       ADC3     192000 uV
  vbat      ADC6    3340000 uV

operations    : writes 33, reads 84
after standby : STANDBY
```

`demo`. The two failures are deliberate: they exercise the error path.

```text
scenario   result adapter    current,µA pump   note
------------------------------------------------------------------------------
HVDCP3P5   ok     HVDCP3     3000000 yes    Quick Charge 3.0, 9 V and 3 A, charge pump possible
HVDCP3     ok     HVDCP3     3000000 yes    Quick Charge 3.0, 9 V and 3 A, charge pump possible
HVDCP2     ok     HVDCP2     1500000 no     Quick Charge 2.0, 9 V and 1.5 A
DCP        ok     DCP        1500000 no     charging-only port, BC1.2 1.5 A
SDP        ok     SDP        500000  no     standard USB port, 500 mA limit
DETACHED   failure -          -       -      error detection_timeout - no power: a timeout is expected
UNKNOWN    failure -          -       -      error unknown_adapter_pattern - unknown pattern: a failure is expected

journal: artifacts\journal-demo.jsonl
```

`verify` ends with:

```text
self-check: passed (policies, current grid, APSD decoding, error path)
```

## How it works

```text
client (IOCTL) ──► KMDF driver ──► logic core ──► transport ──► \Device\RESOURCE_HUB (SPMI) ──► SMB in PM8150B
                                     │
                                     ├─ reads APSD_STATUS / APSD_RESULT_STATUS
                                     ├─ decodes the adapter type (table from Android)
                                     └─ writes the input current limit and the QC2 voltage
```

The core knows nothing about Windows and nothing about I/O: it works on top of the
[`ChargerTransport`](crates/core/src/transport.rs) trait, and it is handed time and
the journal from the outside. That is why all of the logic, timeouts and failure
recovery included, is testable without hardware.

More detail: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
[docs/REGISTERS.md](docs/REGISTERS.md), [docs/LN8000.md](docs/LN8000.md).

## Limitations and honest caveats

* **The IOCTL surface of the SMB driver is not wired up yet.** `GET_STATUS`
  answers. `READ_REG` returns a payload whose `error_code` is
  `STATUS_NOT_IMPLEMENTED`, and `WRITE_REG`, `SET_ICL` and `GET_JOURNAL` return
  `STATUS_NOT_IMPLEMENTED` outright. Detection and policy application run on the
  bring-up timer, so `DETECT_START` and `APPLY_POLICY` answer the same way. The
  logic behind those entry points is written and covered by mock-based tests; what
  is missing is the kernel-side plumbing.
* **The SPMI response layout is not confirmed by reverse engineering.** The
  transport builds a register read as "16-bit address, then one byte" and the
  driver holds a live `Charger` over it, but the framing of the bus response has
  not been proven, so [docs/REGISTERS.md](docs/REGISTERS.md) and
  [docs/SPMI-PATH.md](docs/SPMI-PATH.md) keep register readings provisional. A
  guess is not presented as a fact.
* **Building the kernel-mode drivers needs the WDK.** `kmdf.sys` is signed, its INF
  passes `infverif`, and the PE machine field confirms 64-bit ARM (`0xAA64`). The
  package is written to `artifacts/driver-arm64/`. LLVM **17.x** is required for
  `bindgen`.
* **PD voltage negotiation stays with the platform's Type-C part.** Without it the
  charge pump can hold an already-negotiated voltage or work in bypass from 5 V,
  but it cannot deliver the full 33 W.
* **The LN8000 driver is deployed and under measurement, not finished.** The KMDF
  driver for the ACPI node `PEIC` (I²C address 0x51) is built and installed on the
  tablet. [docs/STATE-2026-09-17.md](docs/STATE-2026-09-17.md) records the defects
  that are still open, and [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md) is the
  install and diagnostics procedure.

## Building the driver

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # LLVM 17.0.6 required

# The SMB detection driver (block detection and input current limit)
cd crates/kmdf        ; cargo wdk build --target-arch arm64 --profile release

# The LN8000 charge pump driver (PEIC node, I2C 0x51)
cd crates/ln8000-kmdf ; cargo wdk build --target-arch arm64 --profile release
```

The packages are written to `artifacts/driver-arm64/` (SMB) and
`artifacts/driver-ln8000-arm64/` (LN8000). Both are produced locally and are not
tracked by git.

Installing and diagnosing the LN8000 - [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md):
`install-driver.ps1`, `nabu-ln8000.ps1 status|sessions|read|write|journal`,
`run-acceptance.ps1` (the automated acceptance protocol) and
`uninstall-driver.ps1`. A PowerShell script that carries non-ASCII text is saved
as UTF-8 with a BOM, because PowerShell 5.1 otherwise decodes it as ANSI and
mis-parses the quoting.

## Licence

Dual: MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
