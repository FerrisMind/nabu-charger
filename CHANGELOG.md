# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/),
and versions follow [SemVer](https://semver.org/lang/ru/).

## [Unreleased]

Four changes to the input-presence and charging decisions, all measured on the
development tablet on 22.09 (`docs/FINDINGS.md`). Two of them are **reversals of fixes
made earlier the same day**: requiring current in the 4.6-6.0 V band, and the 4.6 V floor
that band was built on. Both were undone because the measurement showed they make a
working brick invisible, and the operator's meter - not the tray - was telling the truth.

### Added

* **Three marks that name the reason for an AC verdict**, so a rejection can be told from a
  sleeping ADC in one dump: `VbatTickMv` is the `vbat_uv` the tick actually used, next to
  `DoubledVeto` (`vin_is_doubled_vbat` on that value) and `OnlineRaw` (1 online, 0 offline,
  2 no evidence). The tick's `vbat_uv` is the ADC's VBAT channel - a converter node, not the
  cell, and it reads `Vin / 2` in switching - so below 6 V the floor, the veto and the
  headroom test all rest on a node whose meaning moves with the pump's state, and a live
  rejection could not be attributed without it.

### Fixed

* **A working 5 V brick was published as "on battery" for a whole session.** Measured with
  the operator's brick delivering ~1.5 A into the pack through the platform's own SMB5
  buck: `Vin = 4 400 000-4 544 000` (the loaded node), cell `4 130 000-4 180 000`,
  `Iin` on the 39 120 uA ADC floor because the pump never reaches its mode, `FAULT1` bit 4
  **clear** (the hardware comparator saying the bus is present), and the tray on
  `BattPwr = 2` with Windows applying the DC idle policy to a tablet sitting on a brick.
  The 4.6 V floor (`VBUS_ONLINE_UV`) is what rejected it, so the floor is gone: below
  `VBUS_ELEVATED_UV` the decision is now the 4.2 V floor plus the headroom over the cell,
  with the comparator having already spoken above it. An unplugged node is not this case -
  with no cable the reflection is `2 · VBAT` (measured 8.46 V at a 4.23 V cell and 8.86 V
  at 4.43 V), which the doubling veto catches first. Windows now sees the adapter
  (`pwr = 1`) on the first tick.

* **The charging flag had no witness at all while the pump is idle.** LN8000 `Iin`
  measures the pump's own input, so when the platform buck carries the charge it sits on
  the ADC floor and `CHARGING` could never be published - the operator's complaint, with
  his meter showing the current. The witness is now the fuel counter's SOC
  (`battery_policy::SocTrend`, fed by the 30 s gauge poll): it rises while the pack fills
  and falls while it drains, and a fall clears the verdict on the spot. The counter's
  *cell current* was rejected for this role on measurement, not on principle: on 22.09 it
  read **negative in both directions** - while the pump pushed ~3.5 A into a pack whose SOC
  climbed `223 -> 234`, and while the pack drained at 0.54 A with the cable out and the SOC
  falling `234 -> 233`. The magnitude matched the cell current in both states; the sign did
  not follow, so the field stays a diagnostic (`FgIbatUa`) and the SOC decides. New marks
  `SocRising` and `SocRiseAgeMs`.

* **A removal took 7-8 s to reach Windows, and every second of it was the hold.** With the
  cable pulled, `FAULT1` bit 4 asserted within one telemetry tick, and the 8 s
  `ONLINE_HOLD_MS` window then kept `POWER_ON_LINE` published until it expired (measured:
  `14:57:30.816 -> 14:57:38.842`, 8.03 s). The tick now ends the online window on the spot
  when the hardware says the input is gone *and* no current is flowing into the pack
  (`crates/ln8000-kmdf/src/battery.rs`). The current test keeps the existing rule that a
  stale bit cannot drop the input while the pack is really charging. The charging hold is
  deliberately left armed: `charging` is gated on `online`, so the flag still drops with it
  and still returns quickly when the cable comes back. **The verdict must hold for three
  consecutive ticks** (`battery_policy::UNPLUG_RELEASE_RUN`): on 22.09 at 20:34 the same bit
  came and went in 290 ms windows every 15-40 s with a working 5 V brick attached, because
  the driver's own engagement pulses collapse the adapter's output, and releasing on the
  first of them kept the tray dark for the whole session. A 290 ms window is longer than the
  250 ms telemetry tick, so *two* samples fit inside one window - three ticks (750 ms) are
  the first run a window cannot produce, and that is the whole cost of a real removal now.
  The SOC trend is deliberately not part of this test: a rise can be up to
  `SOC_RISE_HOLD_MS` old, and a history cannot outvote the live verdict of a removal.

* **The 4.6-6.0 V band published AC for an input that was not there** - measured
  4 928 000-5 072 000 uV with the pump engaged and `Iin` on the 39 120 uA ADC floor, cable
  out, five seconds of it. Requiring current in that band was tried and **reverted the same
  day** (see the bullet above): with this brick the pump never reaches its mode, so no
  current ever flows and the band answered "no adapter" forever while the tablet charged.
  The band stays voltage-only; the phantom route through it stays open and is documented as
  such in `docs/FINDINGS.md`, because a single tick cannot separate the two cases.

* **A hibernated ADC made an insertion invisible, with nothing in the driver able to wake
  it.** The LN8000 ADC leaves initialisation in `AutoHibernate` with the `Sec4` delay, so
  about four seconds of pump idle put it to sleep; a sleeping ADC reads *successfully* with
  `0x00` in every channel, so `Vin` is zero, `sample.input_present` is false, and charging,
  HVDCP and the online verdict all see "no adapter" - while `ADC_CTRL` is written only
  inside `Pump::configure()`, which runs at device start and on recovery paths that need a
  live sample to be reached at all. Measured after the 20.47.10.666 install: `AdcValid` bit
  0 set, `SuVinUv = 0`, `SuIin = 0`, `BattVbat = 0` with the telemetry tick demonstrably
  alive (`SocAgeMs` advancing between two reads 6 s apart). The tick now uses the hardware's
  own VBUS comparator, which keeps working while the ADC sleeps
  (`battery_policy::adc_wake_needed`): VBUS present with unusable samples wakes the chip
  (`Pump::set_adc_mode(AdcMode::Normal)`), VBUS gone returns it to `AutoHibernate` so the
  idle current comes back. New marks `AdcAwake` and `AdcWakeN` make both visible in a dump.
  The 20:34 event shows the chip waking on the brick by itself (`usable = 1` with
  `awake = 0` before any write), so this is a safety net rather than the cure it first
  looked like.

### Changed

* Driver version **20.47.10.672** (package version 0.3.1). It has to outrank the
  `20.47.10.671` package on the development tablet: a lower DriverVer is refused as
  "Outranked".

## [0.3.0] - 2026-09-22

The first release with an installable ARM64 package: driver **20.47.10.665**, built from
this tree and published as `nabu-ln8000-driver-0.3.0-arm64.zip`. The version is a minor
bump rather than a patch because the access policy below is a breaking change for any
client that opened the pump without elevation.

### Added

* **An access policy on the device object.** Every control code is `FILE_ANY_ACCESS` and
  the driver made no requestor check, so any process on the tablet could open
  `\\.\nabu_ln8000` and drive the pump - `SET_MODE`, `SET_CHARGE` and `WRITE_REG` are
  not diagnostics. The driver now applies `D:P(A;;GA;;;SY)(A;;GA;;;BA)` to the device
  object (`crates/ln8000-kmdf/src/sddl.rs`), and the INF sets the same descriptor on the
  device node, whose object is created by the ACPI bus driver and therefore does not
  carry the driver's. The user-mode tools in `deploy/` now need an elevated session;
  reading the telemetry marks does not, because those are registry values.
* `docs/FINDINGS.md`: the six hardware findings this project produced, each with the code
  or the measurement it rests on, plus the defects that are still open and what a reader
  can do with the findings.
* **A project status section in all three READMEs** - what works and what does not, with
  the measurement behind every row - and build-and-install instructions, including the
  two requirements that were previously written down nowhere: test signing has to be on
  and Secure Boot off, because the packages are test-signed.
* A release archive assembled by `deploy/build-arm64.ps1` and the procedure in
  [docs/RELEASE.md](docs/RELEASE.md).

### Changed

* **The repository is licensed GPL-2.0-or-later** (was `MIT OR Apache-2.0`). A
  provenance audit compared `crates/ln8000` against the GPL-2.0-or-later Android kernel
  driver it was written from, function by function: the register map, the bit masks, the
  numeric codes, the initialisation sequence and six encoding functions are a port of it,
  and `crates/ln8000-kmdf` links that crate. The previous position, argued in
  `reference/README.md`, was that the pinned reference file is never compiled and so does
  not affect the licence of the code; that argument was wrong, because the same content
  enters the build through `regs.rs` regardless. What came from where is written out in
  [PROVENANCE.md](PROVENANCE.md).
* One comment in `crates/core/src/apsd.rs` had been copied verbatim, misspelling
  included, from `smb5-lib.c`; it is rewritten in this project's own words.
* **Every crate now carries the same version.** The workspace was at `0.1.0`, `spb` and
  `ln8000-kmdf` at `0.2.0`, and `cli`/`host` pinned `charger-core` at `0.1.0` by hand, so
  a version bump in one place could not be made without editing five files that nothing
  kept in step. `spb` now inherits the workspace version and the two hand-written pins are
  gone. The Cargo version tracks the project; the Windows driver version
  (`20.47.10.665`) is stamped separately by `STAMPINF_VERSION` and has to outrank what
  the tablet already has, or the install is refused as `Outranked`.
* The README no longer quotes `docs/STATE-2026-09-17.md` as the current state; that page
  is marked as superseded in part, because its headline claim ("charging under Windows
  does not work") stopped being true when the access path to the node was solved.

### Fixed

* `docs/ACCEPTANCE.md` recorded `LLVM 23` as the toolchain, which contradicts the LLVM
  **17.0.6** the kernel drivers actually need; the line now says which checks need LLVM at
  all and which version.
* **The access policy did not work at first, and the fault was not in the SDDL string.**
  Measured on the tablet: the assignment returned success - the `SddlSt` mark read `0` -
  and `WdfDeviceCreate` then failed with `STATUS_INVALID_SECURITY_DESCR` (`0xC0000079`),
  which the node reported as `CM_PROB_FAILED_ADD` (31). A security descriptor cannot be
  attached to an **unnamed** device object: `WdfDeviceInitAssignSDDLString` requires the
  driver to name the object first, or to call `WdfDeviceInitSetCharacteristics` with
  `FILE_AUTOGENERATED_DEVICE_NAME`, and a PnP device must not name its own object. The
  driver now asks for the autogenerated name before it assigns the descriptor, and the
  device started on the first install afterwards - driver stage mark `STAGE_DONE`, node
  `OK`, no reboot required. Two earlier hypotheses were **wrong** and are recorded here so
  that nobody re-derives them: a missing NUL terminator in the UTF-16 buffer, and a `const`
  array whose temporary does not outlive the call. The terminator and the `static` remain in
  the code because they match the canonical `DECLARE_CONST_UNICODE_STRING` form, not because
  either of them was the fix.
* **`deploy/nabu-ln8000.ps1` could not send a buffer, so `status`, `sessions`, `read` and
  `write` all failed.** Three defects, none of them visible on the development host. The
  buffer parameter was named `Input`, which collides with PowerShell's automatic `$Input`
  - the enumerator of the incoming pipeline - so binding `-Input <byte[]>` failed with
  "cannot convert ArrayListEnumeratorSimple to System.Byte[]" before the body ran.
  `Marshal::SizeOf($Type)` and `Marshal::PtrToStructure($ptr, $Type)` bind to the `object`
  overloads and then try to marshal the `RuntimeType` itself. The parameter is now
  `-Buffer`, `SizeOf` receives an instance, and the generic `PtrToStructure[T]` is reached
  by reflection. Verified on the tablet: `status` prints the mode, the session state, the
  fault registers, the counters and the ADC fields.
* **The reproducibility check compared a file with itself.** `deploy/build-arm64.ps1`
  hashed the `.sys` files in `artifacts/` before the second build and hashed the same files
  again afterwards, but the second build writes into the crate's `target/` tree and nothing
  copies it back - so the verdict was a file against itself and the manifest always said
  `true`. It now reads the package that the second build produced and compares the images
  byte by byte, ignoring the Authenticode certificate table and the PE checksum, which are
  the only places a timestamped signature differs. Measured across repeated builds: every
  section of the image is identical every time, and the binary installed on the tablet
  differs from the binary in the release archive in **nothing but** the signature and the
  checksum - so the artifact a release carries is the code that was verified on hardware.
* **The first version of the fixed check over-claimed.** It printed "bit-for-bit identical
  between builds, signature included" whenever `Compare-PeImage` found no difference
  outside the certificate table and the checksum - but that helper ignores those two
  fields, so its zero was never proof of equality, and reading it as proof is the same
  mistake as the one above in a new coat. Measured against the previous build's bytes:
  512 of the signature's 7168 bytes and 2 checksum bytes differ, so the images are
  identical and the signatures are not. The block now hashes the full bytes as well and
  reports the three cases separately (identical, signature-only, not reproducible), and
  `reproducible_note` in the manifest says which one occurred.
* **And the second build was still a cache hit.** Both rebuilds finished in 0.12 s:
  cargo had nothing to do, so even with the comparison fixed the check could only ever
  witness a re-signature. `cargo clean -p <name>` does not help - it left what
  cargo-wdk's nested package project reuses, reporting "Removed 0 files" for one of the
  two crates. The block now deletes `target\aarch64-pc-windows-msvc\release` before the
  second build and aborts unless that build prints `Compiling <crate>`, so a cache hit
  fails the check instead of passing it. With the cache gone the image still comes out
  identical and the signature does not, which is the claim the manifest makes.

### Known limitations

* **The AC-verdict flap behind the brightness reset is open.** It is the ❌ row of the
  README status table and the release notes repeat it, because a release note that omits
  the one defect a user is most likely to notice would be the only dishonest document in
  this repository.
* **In standby the ADC channels read zero.** Measured on 2026-09-22 with the driver in
  `STANDBY` (mode 1, no critical fault): the registers and counters are live - `samples`
  reached 1238 - while `input current`, `battery voltage` and `input voltage` came back as
  `0`, and the die temperature as `1600` (tenths of °C). A zero raw ADC code decodes to
  exactly `+160.0 °C` (`crates/ln8000-kmdf/src/lib.rs`), so this is the signature of the
  ADC not producing a reading, and the driver's protection treats it as unusable
  (`guard::die_temp_usable`). The fields are only meaningful during a charge session; they
  are recorded here because the diagnostic tool now prints them.

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
