# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/),
and versions follow [SemVer](https://semver.org/lang/ru/).

## [0.2.2] - 2026-09-17

### Fixed (after the first run on the tablet)

The first installation on a real Xiaomi Pad 5 (Windows 11 25H2, ARM64) uncovered seven
defects that are invisible both in the tests and on the host.

* **Driver package**: the WDK certificate was not added to the trusted stores -
  `pnputil` failed with `0x800B0109`. Now `install-driver.ps1` installs it into
  `Root` and `TrustedPublisher`.
* **Code 3010 is no longer treated as an error**: `pnputil /add-driver /install`
  returns it when the package is installed and a reboot is required.
* **INF in ASCII**: Cyrillic and the em dash are read by setupapi as ANSI and
  turn into garbage in the device name. The check is 0 non-ASCII lines.
* **Symlink name**: the buffer was 20 characters for a 23-character name, so the name
  was silently truncated to `\DosDevices\nabu_ln8`. The width is fixed, the length is
  computed from the name, and a compile-time check was added (`assert!` in a const).
* **`WdfDeviceCreate`**: our attribute structure was rejected with
  `STATUS_WDF_OBJECT_ATTRIBUTES_INVALID` (`0xC0200209`), so the device
  did not start (code 31, `CM_PROB_FAILED_ADD`). The device is created without
  attributes - that variant is standard for WDF.
* **`WdfTimerCreate`**: without an owner WDF fails
  (`STATUS_WDF_PARENT_NOT_SPECIFIED`, `0xC0200212`) - the device is set as the owner.

### Known limitations

* **The telemetry timer is not created** (`STATUS_NOT_SUPPORTED`, `0xC00000BB`):
  automatic serialization requires the owner to be DISPATCH-compatible, while
  bus transfers need PASSIVE. The right solution is a DISPATCH-level timer
  with deferred work on PASSIVE; for now the timer failure is not critical:
  control and diagnostics work on demand.
* **LN8000 is not detected**: `_CRS` is parsed, the SPB bus opens, but
  `Pump::open` gets no response from the chip. Next: capture the raw transfer
  status and the `DEVICE_ID` value, check the addressing (7-bit 0x51 vs 8-bit
  0xA2) and whether the chip is held in reset.

### Added

* **Stage marks in the registry**: the driver writes startup progress into the service
  `Parameters` (`DriverStage`/`DriverStatus`, `AttrSize`, `LinkStatus`) and into the device
  key (`AddStage`/`AddStatus`). Without a debugger this is the only way to see a failure
  remotely - and they are exactly what named both crashes (`0xC0200209`, `0xC0200212`).
* **Remote working tools**: `13-remote/remote-ps.ps1` (runs a script on the
  tablet via `EncodedCommand`), a helper that passes the password through
  `SSH_ASKPASS`, and the reconnaissance, installation and check scripts.

## [0.2.0] - 2026-09-16

### Added

* **KMDF charge pump driver for LN8000** (`crates/ln8000-kmdf`): attaches to the
  ACPI node `PEIC` (`QCOM057E`, I²C 0x51), opens the bus through Resource Hub
  (`\Device\RESOURCE_HUB\<16 hex>`) and exchanges a transfer list with the chip via
  `IOCTL_SPB_EXECUTE_SEQUENCE`. It polls `DEVICE_ID`, configures thresholds and
  protections, enables 2:1 mode with a `SYS_STS` check, with a fallback to bypass on failure.
* **Telemetry and session journal** (`ln8000::Telemetry`): a ring of samples
  (voltages, current, temperature, mode) and a charge session history with peak
  values and an accelerated-mode flag; no allocations, rotation in place.
* **Temperature and current protection** (`ln8000::guard`): a decision from the
  latest sample - lower the current, fall back to bypass, or stop charging.
* **Deployment**: an INF package with a test signature (built by `cargo wdk build`),
  the scripts `install-driver.ps1`, `update-driver.ps1`, `uninstall-driver.ps1` and
  the diagnostic client `nabu-ln8000.ps1` (status/sessions/read/write/journal).
* **Documentation**: `docs/DEPLOY-LN8000.md` (installation, update, rollback,
  profiles, risk register, 10-point acceptance protocol), `docs/COMPAT-MATRIX.md`,
  `docs/LOG-FIELDS.md` (units of all fields), `docs/PROTOKOL-PRIEMKI-LN8000.html`.
* **Automatic acceptance protocol** (`deploy/run-acceptance.ps1`): a dialogue
  over ten points, telemetry capture, power gain calculation, output to HTML, Markdown
  and JSON with the journal attached; the `-DryRun` mode checks the pipeline without hardware.
* **Reproducible build** (`deploy/build-arm64.ps1`): builds and signs both
  drivers, copies the package, `SHA256SUMS.txt` and `BUILD-MANIFEST.json` with the tool versions.
* All scripts are saved as UTF-8 **with BOM** - otherwise Windows PowerShell 5.1 corrupts Cyrillic.
* **Cross-check against reference sources** (`deploy/verify-sources.ps1`): decodes the
  I²C descriptor from a live ACPI dump and cross-checks the SMB addresses and bits with
  `smb5-reg.h`, and the LN8000 numeric constants with `ln8000_charger.h`. Result:
  43 checks, 0 discrepancies (`artifacts/verify-sources.txt`). The script explicitly
  tracks chip-generation precedence: the references hold `smb5` and `smb` at the same
  time, and disagree in 31 names - for SM8150/PM8150B `smb5` is correct (the code confirms).
* **The reproducibility check is built into the pipeline**: `build-arm64.ps1` builds
  each driver twice and compares the bytes, writing the verdict into
  `artifacts/BUILD-MANIFEST.json` (the `reproducible` field). Previously this was
  asserted without a check - now it is a measurable quantity: with no changes to the
  code and flags a second run gives the same files, after an edit - different ones (the
  image metadata changes). The checksums always describe exactly the files in the package.
* **Fixed an ADC read defect**: while the two sample bytes are read, the update
  pause is now set (`TIMER_CTRL`, bit 1) and released afterwards - otherwise the bytes
  could come from different conversions and the temperature would be garbage.
  The reference driver does the same; the behavior is pinned by the test
  `adc_read_pauses_and_resumes_conversion_update`.
* **Standalone package check** (`deploy/verify-package.ps1`): 35 checks without
  hardware - contents, bitness, INF, checksums, syntax; the acceptance gate at handover.
* **A heating scenario from the sensor to the registers** (`crates/ln8000/tests/thermal.rs`):
  two integration tests - a temperature rise passes through all three protection levels
  and actually changes the chip registers; after cooling, 2:1 mode is restored. The tests
  revealed a physical effect: the ADC register pairs overlap (battery voltage - `0x0E-0x0F`,
  temperature - `0x0F-0x10`), so a sample of one channel is distorted by a write to the
  neighbor; this is taken into account in the test and in the documentation.
* **The initialization sequence is cross-checked against the reference**: the
  `LION_CTRL = 0xC6` unlock is needed only for a soft reset (which is what we do), the
  reset order matches (unlock -> `BC_OP_2` bit 0 -> pause >=10 ms), and mode switching
  is encoded the same way: the mask `BIT(3)|BIT(0)`, the values standby `0b1000`,
  bypass `0b0001`, switching `0b0000`. These same semantic values are now
  checked mechanically in `verify-sources.ps1` (80 checks in total, 0 discrepancies).
* **A single run of all checks** (`deploy/check-all.ps1`): 7 steps - formatting,
  linter, tests, no-std build, driver build with a reproducibility check,
  package and cross-check against the sources. The verdict goes to `artifacts/check-all.txt`.
* **The watchdog timer is brought into working state**: servicing was added
  (`Pump::service_watchdog`, it refreshes only bit 7 of the `TIMER_CTRL` register),
  covered by two tests and wired into the driver timer. By default the watchdog is off
  - the same as in the reference driver (there it is turned off during initialization);
  if enabled in the profile, it becomes a protection: if the driver hangs, the chip
  stops charging by itself within 5-40 s.
* **A discrepancy with the tablet's Device Tree was found**: the DTS disables five
  protections and two monitors in the pump (`tdie-prot-disable`, `iin-ocp-disable`,
  `iin-reg-disable`, `tdie-reg-disable`, `vbat-reg-disable`, `tbus-mon-disable`,
  `tbat-mon-disable`) - in the Android scheme the main charger does the regulation, and
  the pump works as a 2:1 cascade. Our code enabled them unconditionally. Now they are
  profile flags: `for_nabu_dts()` (the base configuration for the tablet, used by
  `for_qc35_class_b()`) and `protective()` (an alternative for the case where under
  Windows the pump itself charges the battery). Both variants are pinned by register-state tests.
* **A bit overlap is recorded**: bit 2 of `REGULATION_CTRL` serves both as the
  `TEMP_MAX_EN` field and as the low bit of the NTC shutdown configuration (`0x3 << 2`).
  On nabu this does not show up, because the die protection is disabled; in the
  `protective` profile the field gets the value `0b11`. An open question for the hardware run.
* **Known limitation**: the driver does not read the INF parameters yet (`IinLimitUa`,
  `VbatFloatUv`, ...) - the profile is set in the code. Changing the profile is one line
  and a rebuild with one command; registry-driven parameters are the next piece of work.
* **Mode fallback is moved into the core and covered by tests**:
  `enable_switching_or_bypass()` returns the mode actually reached (2:1 or bypass), and
  an error if the chip confirms neither; the driver then puts the chip into standby.
  Three tests, including a simulation of "the chip does not hear the command".
* **A hidden failure was found and fixed**: `enable_bypass()` did not check that the
  chip had actually entered bypass, and on a silent failure it returned "success" - the
  driver would have considered the backup mode enabled while charging was not
  happening. The check is now the same as for 2:1 mode, and there is a test for this case.
* **The configuration profile is read from the registry**: the INF parameters
  (`IinLimitUa`, `VbatFloatUv`, `VacOvpUv`, `NtcAlarmCfg`, `WatchdogEnabled`,
  `ProtectionProfile`, `TelemetryMs`) are applied by the driver at startup from the key
  `<device>\Device Parameters\Parameters`. The bounds and the substitution are moved
  into `PumpConfig::apply_parameter` and covered by tests: a wrong value is rejected
  without touching the profile. Thresholds and protection profile now change with **no rebuild**.
* **The limitation is lifted**: previously the INF parameters were declared, but the
  driver did not read them - the profile was set only in the code.
* **Temperature limits and protection currents became parameters**: `TempReduceDc`,
  `TempBypassDc`, `TempStopDc`, `IinMaxUa`, `IinTargetUa`, `IinFloorUa`,
  `VbatReduceUv`, `BusRetryCount`. The consistency of the set is checked: if the
  current reduction comes later than the fallback to bypass, or the target is above
  the maximum, the whole set is rejected rather than applied partially. The
  `ThermalLimitUv` stub, which did nothing, is removed.
* **The journal is aligned with the acceptance criteria**: the record now has `soc_percent`
  and `battery_status` (from `Win32_Battery`) and `pd_status` with an explanation - the
  pump does not know this data, the OS and an external measurement give it. The record has
  19 fields in total; the composition was checked by running the real record code on a
  device stub (`artifacts/journal-shape.jsonl`), the field description is in `docs/LOG-FIELDS.md`.
* **LN8000 address cross-check against a pinned source**: an excerpt from the Xiaomi
  kernel header (commit `9e94307b9c1994cba25849099a0582ad3ab00390`) with the register
  address definitions is placed in `reference/`; all 31 addresses matched.
* 42 tests in the LN8000 core (was 29): sessions, rotation, thermal scenarios.

* **The access path to the SPMI registers is confirmed by reverse engineering**: the
  stock Qualcomm client reads and writes PMIC registers with the code `0x41808` - this
  is the public `IOCTL_SPB_EXECUTE_SEQUENCE`, not a private protocol. The analysis with
  addresses and excerpts is in `docs/SPMI-PATH.md`.
* **Shared crate `crates/spb`**: the SPB ABI, transfer list assembly and Resource Hub
  path construction in one place, 10 host tests. The tests caught an error in the list
  size arithmetic: the header already includes the first transfer.
* **The SMB driver is switched to the public ABI**: instead of a guess, the hub
  receives a real `SPB_TRANSFER_LIST`; the address byte order is moved into the configuration.

### Known limitations

* The check on a physical tablet has not been performed yet - the acceptance protocol
  is prepared but not filled with measurements.
* All seven control codes are implemented: state, register read and write, limits,
  mode, sessions and sample dump. There are no unresolved stubs in the driver.
* The SPMI connection step (`0x32C004`) is **not performed** in the SMB driver: the
  stock client obtains its I/O target through it, while our driver opens the hub by
  name. The right solution is to take the connection from the `_CRS` of our own node,
  which needs a resource preparation callback. Details are in `docs/SPMI-PATH.md`.
* The register address byte order (`address_big_endian`) is taken from the SPMI
  specification and awaits confirmation on hardware.

## [0.1.0] - 2026-09-16

The first working version of the charging driver core for Xiaomi Pad 5 (nabu).

### Added

* The **LN8000** charge pump driver core (`crates/ln8000`): register map, modes
  (standby / bypass 1:1 / switching 2:1), current and voltage encoding formulas,
  protection and fault decoding, 10-bit ADC readings, software reset and standby
  entry. 29 tests, including faults, a failed bus and a failed write verification.
  The source of the logic is the GPL Android driver (`ln8000_charger.c/.h`), details:
  `docs/LN8000.md`.
* The command `nabu-charger pump --profile qc35|conservative` - runs the charge pump
  on a mock I²C bus with a report on the mode and the ADC channels.
* The `charger-core` logic core without `std` and without `unsafe`: APSD result
  decoding by the table from the reference Android driver, input current policy,
  current encoding grid, session states, timeouts, retries, journal.
* Transport as the `ChargerTransport` trait plus three implementations: a mock
  (`testkit::ScriptedMockTransport`), a mock with a transaction journal
  (`host::mock::MockTransport`), and real TCP (`host::tcp::TcpTransport`).
* A device simulator (`host::sim::Simulator`): a TCP server with SMB registers,
  suitable for checking the transport without hardware.
* The `nabu-charger` CLI with the subcommands `demo`, `detect`, `sim`, `verify`.
* Operation journal: records with a sequence number, a timestamp, a request ID
  and a result; output to JSON Lines and to `tracing`.
* 83 automated tests (37 unit SMB core, 29 unit LN8000 core, 9 integration, 4 CLI,
  doctests), including a detection timeout, a broken connection, an unknown pattern,
  a failed write verification, re-initialization and a chip failure to enable 2:1 mode.
* `criterion` benchmarks: a full session, APSD decoding, the register read/write
  round trip on the mock and over TCP.
* Kernel-mode driver `kmdf` (KMDF, ARM64): the device, a sequential queue of control
  requests, transport to the SPMI bus `\Device\RESOURCE_HUB`, and an IOCTL contract
  for user mode. **Builds and signs for ARM64**:
  `cargo wdk build --target-arch arm64 --profile release` -> `kmdf.sys` (48.5 KB),
  `kmdf.inf`, `kmdf.cat`; `infverif` - "INF is valid"; PE Machine = `0xAA64`.
* CI configuration: `fmt`, `clippy -D warnings`, release build, tests,
  documentation, a build without `std`.

### Known limitations

* The SPMI transport read path awaits confirmation of the bus response layout (reverse engineering).
* The detection timer in the driver is not yet wired to the queue: the IOCTLs
  `DETECT_START`/`APPLY_POLICY`/`GET_JOURNAL` return `STATUS_NOT_IMPLEMENTED`.
* Building `kmdf` requires LLVM **17.0.6** (not 23.x) - see `docs/HANDOVER.md`.
* There is no LN8000 driver on top of the I²C bus under Windows yet: the logic core
  is ready and checked, a KMDF SpbCx module for the ACPI node `PEIC` is needed.
* LN8000 charge pump control: **the logic core is added** (see above); what remains is
  the KMDF module on SpbCx for the ACPI node `PEIC` and a check on the tablet.
