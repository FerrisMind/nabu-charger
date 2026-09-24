# nabu-fastcharge

**English** | [Русский](README.ru.md) | [Português (Brasil)](README.pt-BR.md)

![Status: experimental preview](https://img.shields.io/badge/status-experimental%20preview-orange?style=for-the-badge) ![Device: Xiaomi Pad 5](https://img.shields.io/badge/device-Xiaomi%20Pad%205-blue?style=for-the-badge) ![Platform: Windows 11 ARM64](https://img.shields.io/badge/platform-Windows%2011%20ARM64-0078D4?style=for-the-badge)

Charging driver for the **Xiaomi Pad 5 (nabu, Snapdragon 860) on Windows on
ARM64**.

On Windows the tablet does not charge from any power supply - neither USB-A
(Quick Charge) nor USB-C (Power Delivery). The cause is not a missing fast-charge
feature: **the PMIC performs hardware adapter detection (APSD), but no Windows
component reads its result**, so the input current limit is never raised and the
charge never starts.

This driver closes exactly that gap: it reads the detection result and applies the
current policy.

The reusable part of the work is not the driver but what was learned about the
hardware: [docs/FINDINGS.md](docs/FINDINGS.md) collects six findings about charging on
this platform, each with the code or the measurement it rests on. Start there if you are
porting Windows to a nabu-class tablet rather than using this driver.

## Project status

Latest release: **0.3.1**, carrying driver **20.47.10.672**. That package
(`nabu-ln8000-driver-0.3.1-arm64.zip`) was built and measured by hand, before this
repository was made public; the releases on this page are produced by the `release`
workflow. The development tablet has moved
past that package and now runs **20.47.10.674** (`oem168.inf`, device node `OK` /
`CM_PROB_NONE`); its two fixes are still unreleased and are described below and in the
changelog. Numbers measured on the shipped 0.3.1 package: a Quick Charge brick negotiated
to 8.3 V, `Iin` 0.23–1.28 A, SoC 196 → 199, the charging flag 338 ms after the verdict, and
the unplug released in 1.1 s.

The previous release, **0.3.0** (driver 20.47.10.665), introduced the access policy verified
there — a process that is not elevated is refused with `ERROR_ACCESS_DENIED` (5).

**At a glance:** ✅ fast charging from a Quick Charge brick · ✅ live pump telemetry ·
⚠️ a Power Delivery supply still does not grow the pack · ⚠️ the AC-verdict flap behind the
backlight reset is fixed in the tree and awaits a release: a tick that reads only the pump's
`2 · VBAT` reflection now carries no verdict, and the removal path stays on the hardware bit
(three ticks, measured at 1.1 s from cable-out) · ⚠️ a ~1 Hz panel-brightness oscillation
seen with the adapter attached is a **separate** defect and is still unexplained

Every row below rests on a measurement on the tablet, not on a passing test. Where a
defect is listed as unfixed, a live capture shows it happening — the evidence is in
[docs/FINDINGS.md](docs/FINDINGS.md) and in the defect list further down.

### Simple status

| Feature | Notes | Status |
|---|---|---|
| 🔌 Fast charging from a Quick Charge / HVDCP brick | Measured: SoC 44 → 85 %, `Iin` 1.13–1.62 A, `Vin` 8.7–9.4 V, pump in 2:1 switching | ✅ |
| 🔎 Adapter detection (APSD) and the current policy | Decode tables and per-adapter limits, cross-checked against the Android driver: 80 checks, 0 discrepancies | ✅ |
| ⚙️ Pump core: registers, modes, protections, ADC, thermal guard | 125 unit tests, 6 integration tests, 2 doctests; cross-checked against the vendor header | ✅ |
| 📈 Live telemetry and the session journal | `Vin`, `Iin`, `VBAT`, die temperature, faults and mode, read from the pump over I²C on the tablet | ✅ |
| 📦 Signed ARM64 package, reproducible build, rollback | Package check 35/35, source check 80/80, rollback verified on the tablet | ✅ |
| 🎛 Thresholds and protection profile from the registry | Changing them takes a registry write and a device restart, not a rebuild | ✅ |
| 🔋 Power Delivery (USB-C) supply | The driver gets AC, but the pack does not gain capacity; the negotiation above 5 V stays with the platform's Type-C part, so the full 33 W is out of reach | ⚠️ |
| 🖥 The SMB driver's IOCTL surface | `GET_STATUS` answers; `READ_REG`, `WRITE_REG`, `SET_ICL`, `GET_JOURNAL`, `DETECT_START`, `APPLY_POLICY` return `STATUS_NOT_IMPLEMENTED`. The logic behind them is written and mock-tested; the kernel-side plumbing is not | ⚠️ |
| 🔬 SPMI response framing | Not confirmed by reverse engineering, so register readings stay provisional | ⚠️ |
| 💡 AC-verdict flap / backlight reset | Fixed in the tree (unreleased, driver `20.47.10.674`), awaiting a release. The 22.09 capture of the flap is in `docs/FINDINGS.md`; the 24.09 build was confirmed live — one no-verdict tick on the pull, DC 1.0 s after the cable, AC in the first sample after the replug | ⚠️ |
| 💡 Panel brightness oscillates about once a second with the adapter attached | Measured 24.09: the OS brightness flips between two fixed levels every 1.1–1.3 s for minutes while the pack charges, with no power-source event, no `Kernel-Power` 105 and the driver's marks constant. Independent of `ADAPTBRIGHT`, `DisplayEnhancementService`, the refresh rate, input and the charge level | ❌ |
| 📱 Other SM8150 devices | Only the Xiaomi Pad 5 has been tested; the driver binds if the node exists at I²C 0x51 | ⚠️ |
| 🧩 A device with an LN8000 but no `PEIC` node in its DSDT | Not supported — this needs an ACPI change | ❌ |

### Known defects, not fixed

All of these are live or code-level findings with a reproduction; none is a guess.

**The AC verdict dropped while the brick was attached** — the one that mattered. When the pump
leaves switching, `Iin` sits on the 39 mA ADC floor and `Vin` relaxes to `2 · VBAT`, which is the
*normal* operating point of a 2:1 pump rather than evidence of absence. The doubled-VBUS veto in
`online_raw` read it as "no adapter", the 8 s hold expired, and Windows saw a power-source change —
which is how the reported backlight reset happened. Measured on the tablet with the cable motionless:
AC → DC → AC in 2.647 s, with `Fault1Sts` bit 4 *clear*, so the hardware's own VBUS detector said the
cable was there, and with every reading fully usable.

**Fixed in the tree, not yet in a release** (driver `20.47.10.674`, `oem168.inf`): a tick that reads
only the reflection carries no verdict, so it cannot age the hold, while a real removal is still ended
by the hardware bit. Confirmed live on 24.09: the pull produced exactly one `OnlineRaw = 2` /
`DoubledVeto = 1` tick, the release followed three ticks later with `FAULT1` bit 4, and Windows saw DC
1.0 s after the cable. The same build carries the latency fix in the table below. The brightness
oscillation measured the same day is a different defect — it runs with no power-source event at all —
and is listed on its own.

| Defect | What it does |
|---|---|
| AC arrives seconds after the cable | The verdict (`OnlineRaw`) is on the first tick, but Windows reads the *next* one and the pump bring-up blocks the tick: measured **8.7 s** from insertion to `pwr = 1` on a Quick Charge brick, 5.6 s on a plain 5 V one. **Fixed in the tree** (unreleased, `20.47.10.674`): the online hold is front-armed, and the 24.09 replug carried the flag in its first sample |
| Panel brightness oscillates about once a second | With the adapter attached and the pack charging, the OS brightness flips between two fixed levels every 1.1–1.3 s, in episodes of minutes, with no power-source event, no `Kernel-Power` 105 and no change in the driver's marks; the `Power` service host burns about one core during an episode. Not `ADAPTBRIGHT` (off and on both flicker), not the refresh rate, not input, not the charge level. Cause not established; the 24.09 record is in `docs/FINDINGS.md` |
| Die temperature is published while the ADC hibernates | `AdcValid` bit 1 is reported for a channel that is asleep, so **160.0 °C** is published and every consumer prints it faithfully |
| LN8000 VBAT reads low | 42–43 mV below the fuel gauge, and that channel feeds the 2:1 gate |
| `EngageState` disagrees with `SuMode` | Publishes 4 (NO_HEADROOM) while `SuMode` stays 3 (switching); mark-only noise |

Two hardware faults on this tablet are unrelated to this driver but visible in its
telemetry: the PMIC TCC node `ACPI\QCOM0582` is in an error state, and `WUDFRd` fails
to load 48 times for the sensor platform `ACPI\QCOM059F`.

The build- and tooling-specific caveats — the WDK requirement, the LLVM version, what
is left to the platform — are in
[Limitations and honest caveats](#limitations-and-honest-caveats).

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
own workspace, because they need the WDK and `cargo-wdk`; the `kernel-driver` job in
`.github/workflows/ci.yml` installs both of those and builds them for ARM64.

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
  tablet. The open defects are listed in the project status section above and in
  [docs/FINDINGS.md](docs/FINDINGS.md); [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md)
  is the install and diagnostics procedure.
  [docs/STATE-2026-09-17.md](docs/STATE-2026-09-17.md) is a dated snapshot that
  predates the working access path - read it for what was ruled out, not for the
  current state.

## Building and installing the driver

The short version is below. The full procedure, the diagnostic tool, the configuration
profiles, the failure table and the risk register are in
[docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md).

### What you need

| Requirement | Version / note |
|---|---|
| Rust | The channel is pinned by `rust-toolchain.toml`; add the target with `rustup target add aarch64-pc-windows-msvc` |
| Windows Driver Kit | WDK 10.0.26100 — the `cargo wdk` build needs it |
| LLVM / libclang | **17.0.6**. It is what generates the `bindgen` bindings, and 23.x breaks the build. Point `LIBCLANG_PATH` at `C:\Program Files\LLVM\bin` |
| cargo-wdk | `cargo install cargo-wdk --locked` |
| The tablet | Windows 11 ARM64 with **test signing on and Secure Boot off**. The drivers are test-signed, so Windows will not load them otherwise |
| Rights | Administrator on the tablet for the install and for the diagnostic tool — the driver's device object is restricted to `LocalSystem` and Administrators |

### Build

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # LLVM 17.0.6 required

# The SMB detection driver (block detection and input current limit)
cd crates/kmdf        ; cargo wdk build --target-arch arm64 --profile release

# The LN8000 charge pump driver (PEIC node, I2C 0x51)
cd crates/ln8000-kmdf ; cargo wdk build --target-arch arm64 --profile release
```

`deploy/build-arm64.ps1` runs both, signs them and writes the checksums and the build
manifest. The packages land in `artifacts/driver-arm64/` (SMB) and
`artifacts/driver-ln8000-arm64/` (LN8000). They are produced locally and are not tracked
by git. The LN8000 is the one with an install procedure. The SMB driver builds for ARM64,
but its IOCTLs are stubs and it has not been deployed on the tablet — the status table
above says which ones answer.

`deploy/assemble-release.ps1 -Version <x.y.z>` packs the installable kit into the release
archive, and that is what the **release** workflow (`.github/workflows/release.yml`) runs:
it starts only by hand, from the Actions tab, and it builds, checks and packs on a runner
and leaves a draft release carrying the archive. The whole procedure — the two version
numbers and why there are two, and the checks that gate a release — is in
[docs/RELEASE.md](docs/RELEASE.md).

### Install on the tablet

On the tablet, as administrator:

```powershell
bcdedit /set testsigning on     # then reboot once
```

Copy `artifacts/driver-ln8000-arm64/` to the tablet, for example to `C:\nabu-ln8000\`,
and run the installer from that folder:

```powershell
cd C:\nabu-ln8000
.\install-driver.ps1           # checks the signing mode, installs, binds ACPI\QCOM057E, starts it
.\nabu-ln8000.ps1 status       # expect: mode SWITCHING 2:1, or BYPASS 1:1 as the safe fallback
```

No ACPI change and no UEFI reflash is needed: the `PEIC` node (`_HID = QCOM057E`, I²C 0x51
on `\_SB.I2C5`) is already described in the tablet's DSDT and the driver binds to it.

### Update and rollback

```powershell
.\update-driver.ps1                 # installs on top, keeps the previous package
.\uninstall-driver.ps1              # stops the service and removes the package
pnputil /add-driver $env:ProgramData\nabu-fastcharge\backup\ln8000_kmdf.inf /install
```

The driver writes nothing to firmware and changes no power settings, so removing it
returns the device to its pre-installation behaviour.

The rest of the toolbox — `nabu-ln8000.ps1 status|sessions|read|write|journal`,
`run-acceptance.ps1` (the automated acceptance protocol) — is described in
[docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md). A PowerShell script that carries
non-ASCII text is saved as UTF-8 with a BOM, because PowerShell 5.1 otherwise decodes it
as ANSI and mis-parses the quoting.

## Licence and access policy

**Licence: GPL-2.0-or-later** ([LICENSE](LICENSE)). The LN8000 logic is a port of the
GPL-2.0-or-later Android kernel driver for the same chip, so this repository cannot be
MIT/Apache. What came from where is written out in [PROVENANCE.md](PROVENANCE.md).

**Access policy: the pump is open to `LocalSystem` and Administrators only.** The driver
sets that descriptor on the device object and the INF sets the same one on the device
node, because every control code is `FILE_ANY_ACCESS` and the driver makes no requestor
check. The tools in `deploy/` therefore have to run elevated; reading the telemetry marks
does not, because those are registry values.
