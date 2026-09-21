# Acceptance report

The check was performed on 16.09.2026 on the machine: Windows 11 (26200), AMD Ryzen 7 3700X,
Rust 1.97.0 (`rust-toolchain.toml` pins the channel), WDK 10.0.26100, LLVM 23.

The raw command output is in `artifacts/`: `verify-fmt.txt`, `verify-clippy.txt`,
`verify-build.txt`, `verify-test.txt`, `verify-doc.txt`, `verify-demo.txt`,
`verify-selfcheck.txt`, `bench-core.txt`, `bench-transport.txt`,
`journal-demo.jsonl`.

## Acceptance criteria

| No | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | Build on edition 2024 without errors and warnings | <span>done</span> | `edition = "2024"` in `Cargo.toml`; `cargo build --workspace --release` - success; `cargo clippy --workspace --all-targets -- -D warnings` - success (the files `verify-build.txt`, `verify-clippy.txt`) |
| 2 | The public API is documented and covered by examples | <span>done</span> | `#![deny(missing_docs)]` in both crates; rustdoc with the "Errors"/"Safety" sections; `cargo doc --workspace --no-deps` without warnings (`verify-doc.txt`) |
| 3 | All tests pass, including the failure scenarios | <span>done</span> | 83 tests: 37 unit in the SMB core, 29 unit in the LN8000 core, 9 integration, 4 CLI, doctests; among them the detection timeout, a lost connection, an unknown pattern, a failed write verification, re-initialization and the chip refusing to enable the 2:1 mode (`verify-test.txt`) |
| 4 | Errors are typed, the normal path does not panic | <span>done</span> | `ChargerError`/`TransportError` implement `core::error::Error`; the lints `clippy::unwrap_used`, `expect_used`, `panic`, `indexing_slicing` are enabled as `deny` in `Cargo.toml` - the build passes, which means they are absent from the library code |
| 5 | The `unsafe` blocks are minimal and justified | <span>done</span> | `charger-core` and `charger-host` have **zero** `unsafe` (`unsafe_code = "deny"` is set in `Cargo.toml`); in `kmdf` the `unsafe` is concentrated in the WDF calls, each with a comment carrying the invariants and the IRQL level |
| 6 | Resources are released, a failure does not leave a hung state | <span>done</span> | `Charger::close` and `Drop` return a safe current limit; the tests `drop_emits_close_event`, `close_sets_safe_current_and_closes_session`, `reinit_restores_session_after_fault`, `reopening_works_without_restarting_the_process` |
| 7 | The driver is not tied to a transport, it is tested on a mock | <span>done</span> | the `ChargerTransport` trait; three implementations (a mock, a mock with a journal, TCP); 9 integration tests run on the mock and the simulator without hardware (`verify-test.txt`) |
| 8 | Every operation is recorded in the journal | <span>done</span> | `Journal`/`Event`: `seq`, `ts_ms`, `request_id`, `level`, `kind` + the operation fields; the test run artifact - `artifacts/journal-demo.jsonl` (289 records), the tests `every_event_has_request_id_and_timestamp`, `journal_records_request_ids_and_monotonic_time` |
| 9 | The demo runs and reproducibly shows the work | <span>done</span> | `cargo run -p cli -- demo` - a table over 7 scenarios, including two expected failures (`verify-demo.txt`) |
| 10 | A measurable baseline performance is recorded | <span>done</span> | `docs/PERFORMANCE.md` + `artifacts/bench-*.txt`: a full session cycle ~0.9 µs, the mock 163 M ops/s, TCP 7 640 ops/s |
| 11 | CI runs the build, the tests, the linters and the documentation | <span>done</span> | `.github/workflows/ci.yml`: `fmt --check`, `clippy -D warnings`, `build --release`, `test`, `doc`, a build without `std`; a separate job for the ARM64 driver |
| 12 | There is a handover and rollback guide | <span>done</span> | `docs/HANDOVER.md`: the architectures, the requirements, the build, semver, the rollback, a table of typical failures, the journal analysis |
| 13 | Kernel-mode driver build for ARM64 | <span>done</span> | `cargo wdk build --target-arch arm64 --profile release` - `Finished building kmdf`; in `artifacts/driver-arm64/`: `kmdf.sys` (48.5 KB), `kmdf.inf`, `kmdf.cat`, the test signing certificate; `infverif` - "INF is valid"; the PE check: Machine = `0xAA64` (ARM64) |
| 14 | The LN8000 charge pump driver core | <span>done</span> | `crates/ln8000`: the register map, the modes, the current/voltage formulas, protection and ADC parsing; builds without `std`; the demo - `artifacts/verify-pump.txt`; the logic is cross-checked with the Android GPL driver: `docs/LN8000.md` |
| 15 | The LN8000 KMDF driver on the PEIC node (I2C) | <span>done</span> | `cargo wdk build --target-arch arm64 --profile release` in `crates/ln8000-kmdf` -> `ln8000_kmdf.sys` (60 KB, SHA-256 `27db2f4468c5e834...`), `.inf`, `.cat`; `infverif` passed; PE Machine = `0xAA64`; the package - `artifacts/driver-ln8000-arm64/` |
| 16 | Deployment, update and rollback | <span>done</span> | `deploy/install-driver.ps1` (the test signing check, `pnputil`, the binding to `ACPI\QCOM057E`), `update-driver.ps1` (keeping the previous package), `uninstall-driver.ps1` (the transcript in `%ProgramData%\nabu-fastcharge\uninstall.log`); all four scripts passed the syntax check |
| 17 | Telemetry and the charge session journal | <span>done</span> | `ln8000::Telemetry`: a ring of 256 samples + a history of 32 sessions, rotation, no allocations; tests for session open/close, rotation, bypass; the export - `nabu-ln8000.ps1 sessions|journal` |
| 18 | Temperature and current protection | <span>done</span> | `ln8000::guard`: three levels (current reduction -> bypass -> stop), tests for the bounds and the strict profile; in the driver the action is applied in the timer and written to the journal |
| 19 | Automatic acceptance protocol | <span>done</span> | `deploy/run-acceptance.ps1`: 10 items, telemetry capture, power gain calculation, output of HTML + Markdown + JSON with the attached journal; the pipeline was verified by a dry run - `artifacts/acceptance-dryrun/` (the report is marked as not a real measurement) |
| 20 | A reproducible signed artifact with a checksum | <span>done</span> | `deploy/build-arm64.ps1`: both drivers, signing, `artifacts/SHA256SUMS.txt` (18 records), `artifacts/BUILD-MANIFEST.json` with the versions (rustc 1.97.0, cargo-wdk 0.1.1, WDK 26100, LLVM bin). **Reproducibility is not declared, it is measured**: the pipeline builds twice and writes the verdict into the manifest (`reproducible`). With the current tool set a repeated run without changes yields the same files (`true`); after a code or flag change the image bytes differ, and that is expected |
| 21 | The shared SPB crate and the layout tests | <span>done</span> | `crates/spb`: the SPB ABI (a 48 byte header, a 32 byte element), transfer list assembly through `offset_of!`, the Resource Hub path; 10 host tests; the tests caught an error in the list size calculation (the header already includes the first transfer) |
| 22 | The access path to the SPMI registers is confirmed by reverse engineering | <span>done</span> | The stock client uses `0x41808` = `IOCTL_SPB_EXECUTE_SEQUENCE`, not a private protocol: `docs/SPMI-PATH.md`, the dumps `re-spmi-*.txt`. The SMB driver was moved to the public ABI and built for ARM64 (49 KB, SHA-256 `fb500868dc1afe80...`) |
| 23 | Cross-check against the reference sources and a live dump | <span>done</span> | `deploy/verify-sources.ps1`: **80 checks, 0 discrepancies** (`artifacts/verify-sources.txt`). The I2C descriptor from the `_CRS` of the PEIC node: the address `0x51`, 100 kHz, the node `\_SB.I2C5`, the EndTag - matched. 13 SMB addresses and 11 SMB bits matched `smb5-reg.h`; the QC2 codes 0x00/0x40/0x80. 31 LN8000 register addresses matched the pinned header; the mode states (`BYPASS`=8, `SWITCHING`=4, `STANDBY`=2, `SHUTDOWN`=1) and the `SYS_CTRL` bits (1<<3, 1<<0) matched; 13 LN8000 numeric constants matched |
| 24 | Standalone package check | <span>done</span> | `deploy/verify-package.ps1`: 35 checks, zero problems - the file set, the `0xAA64` bitness, the INF contents, the checksums, the script parsing |
| 25 | The heating scenario from the sensor to the registers | <span>done</span> | `crates/ln8000/tests/thermal.rs`: two tests - a temperature rise goes through all three levels (normal -> current reduction -> bypass -> stop) and **changes the chip registers**; after cooling the 2:1 mode is restored. As a side effect a real effect was found: the ADC register pairs overlap (`VBAT` is read from `0x0E-0x0F`, `DIETEMP` from `0x0F-0x10`), so writing one channel distorts the neighboring sample |
| 26 | A single run of all checks | <span>done</span> | `deploy/check-all.ps1`: 7 steps (formatting, the linter, the tests, a build without std, the ARM64 build with the reproducibility check, the package, the sources); the verdict and the report - `artifacts/check-all.txt`, the return code 0. The current run: everything matched, 110 tests |
| 27 | Watchdog timer: enabling and servicing | <span>done</span> | `Pump::service_watchdog()` refreshes only bit 7 of `TIMER_CTRL` and does not touch the period bits; two tests (an enabled watchdog is serviced, a disabled one does not enable itself) and a call from the driver timer. Disabled by default - as in the reference driver, which turns the watchdog off at initialization |
| 28 | Pump protections: matching the tablet Device Tree | <span>done</span> | `PumpConfig::for_nabu_dts()` mirrors `nabu-sm8150.dtsi`: `vbat-reg`, `iin-ocp`, `iin-reg`, `tdie-prot`, `tdie-reg`, `tbus-mon`, `tbat-mon` are disabled; `for_qc35_class_b()` uses that base, `protective()` is the alternative with the loops enabled. Two tests check the bits of `REGULATION_CTRL`, `FAULT_CTRL`, `RECOVERY_CTRL` and the preservation of the NTC field |
| 29 | Mode rollback: 2:1 -> bypass -> standby | <span>done</span> | `enable_switching_or_bypass()` in the core + three tests: the 2:1 mode is confirmed; the chip "does not hear" 2:1 -> bypass engages and bit 1:1 is set in `SYS_CTRL`; nothing is confirmed -> a mode error is returned and the chip is put into standby. The tests revealed that `enable_bypass()` did not check the chip response - fixed |
| 30 | Journal: state of charge and negotiation status | <span>done</span> | The journal record was extended to 19 fields: `soc_percent` and `battery_status` (from `Win32_Battery`), `pd_status` with an explanation ( `1` - 5 V, `2` - 9 V and above, `3` - QC), `exported_at`, the mode, the faults, current/voltage/temperature, the counters. Verified by running the real record-writing code against a device stub: `artifacts/journal-shape.jsonl` |
| 31 | The configuration profile is read from the registry | <span>done</span> | The driver reads `Parameters` from the device hardware key: `IinLimitUa`, `VbatFloatUv`, `VacOvpUv`, `NtcAlarmCfg`, `WatchdogEnabled`, `ProtectionProfile`, `TelemetryMs`. The bounds and the substitution are checked in `PumpConfig::apply_parameter` and covered by tests: known values are applied, out-of-range and unknown names are rejected without changing the profile. Changing the thresholds and the protection profile takes **no rebuild**, the registry and a device restart are enough |
| 32 | The temperature limits and the protection currents are configurable | <span>done</span> | `GuardLimits::apply_parameter` accepts `TempReduceDc`, `TempBypassDc`, `TempStopDc`, `IinMaxUa`, `IinTargetUa`, `IinFloorUa`, `VbatReduceUv`; plus `BusRetryCount` for the bus retries. The consistency check (`is_consistent`) rejects a meaningless set as a whole - a partially updated threshold set never happens. Two tests; the `ThermalLimitUv` parameter, which did nothing, is replaced by the real thresholds |

## What was checked separately

**The expected failures work as intended.** The demo shows not only the successful
paths: the "no power" scenario ends in `detection_timeout`, and
"unknown pattern" in `unknown_adapter_pattern`. The driver does not invent an adapter
type when the hardware is silent.

**The core builds without `std`.** `cargo build -p charger-core --no-default-features`
passes - this is what the kernel-mode driver needs.

**The journal is fit for incident analysis.** In `artifacts/journal-demo.jsonl` the
records show the whole session: the open, the register reads, the detection with the raw
values, the applied policy, the retries and the errors.

## What is not done and why

| Item | State | Reason and analysis |
|---|---|---|
| The SPMI bus read path | returns a typed failure | the layout of the `IOCTL_RESOURCE_HUB_TRANSACT` response is not fully confirmed by reverse engineering. A guess is not passed off as a fact |
| The detection timer and the journal delivery to the client | written, but not wired to the queue | the driver code builds and signs, but there has been no live run on the tablet yet: the IOCTLs `DETECT_START`/`APPLY_POLICY`/`GET_JOURNAL` return `STATUS_NOT_IMPLEMENTED` |
| LN8000 charge pump control | the core is ready, the driver is not | the logic is ported from `ln8000_charger.c` and verified by 29 tests; a KMDF module on SpbCx is needed for the ACPI node `PEIC` (address 0x51) |

## How the ARM64 driver was produced (evidence)

```text
cargo wdk build --target-arch arm64 --profile release
INFO  Building package kmdf
INFO  Running stampinf
INFO  Running inf2cat
INFO  Signing kmdf.sys using signtool
INFO  Signing kmdf.cat using signtool
INFO  Running infverif
INFO  Finished building kmdf

artifacts/driver-arm64/
  kmdf.sys   48.5 KB   (PE Machine = 0xAA64 -> ARM64)
  kmdf.inf    2.4 KB
  kmdf.cat    7.9 KB
  WDRLocalTestCert.cer
```

## Summary

The driver core is ready and verified: 83 tests, clean linters, documentation,
benchmarks, the journal and the handover guide. The logic that enables charging
(the APSD read and the current limit) is implemented and reproducible on mocks. The
LN8000 charge pump driver core was added - the second stage for the full charge current.

**The kernel-mode driver builds for ARM64**: `kmdf.sys` is built, signed with the test
certificate, the INF passed `infverif`, and the bitness is confirmed by the PE header
(`0xAA64`).

Next steps: wire the detection timer to the queue, finish reverse engineering the layout
of the SPMI bus response for reads, and write the SpbCx KMDF module for the LN8000.
