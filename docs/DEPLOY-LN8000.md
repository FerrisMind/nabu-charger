# LN8000: deployment, diagnostics, rollback

A practical guide to the LN8000 charge pump driver for the tablet
Xiaomi Pad 5 (`nabu`) on Windows on ARM64.

Related documents: [LN8000.md](LN8000.md) (register map and modes),
[ARCHITECTURE.md](ARCHITECTURE.md), [HANDOVER.md](HANDOVER.md).

---

## 1. What is installed and where

| Component | Path | Purpose |
|---|---|---|
| Driver | `crates/ln8000-kmdf` -> `ln8000_kmdf.sys` + `.inf` + `.cat` | drives the LN8000 on the ACPI node `PEIC` |
| Ready package | `artifacts/driver-ln8000-arm64/` | what is copied to the tablet |
| Deployment scripts | `crates/ln8000-kmdf/deploy/` | installation, update, removal, diagnostics |

The `PEIC` node (`_HID = QCOM057E`, I²C 0x51 on `\_SB.I2C5`, `_STA = 0x0F`) **is already
described in the tablet DSDT** - no ACPI changes and no UEFI reflash are needed. The driver
binds to the existing `_HID`.

---

## 2. Build

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"     # LLVM 17.0.6 required

cd crates/ln8000-kmdf
cargo wdk build --target-arch arm64 --profile release
```

Result: `target/aarch64-pc-windows-msvc/release/ln8000_kmdf_package/`
(`ln8000_kmdf.sys` 60 KB, `ln8000_kmdf.inf`, `ln8000_kmdf.cat`, the test signing
certificate). The checksum and the build manifest - `deploy/build-arm64.ps1`,
the result in `artifacts/SHA256SUMS.txt` and `artifacts/BUILD-MANIFEST.json`.
Bitness check:

```powershell
$sys = "target/aarch64-pc-windows-msvc/release/ln8000_kmdf.sys"
$b = [IO.File]::ReadAllBytes($sys); $pe = [BitConverter]::ToInt32($b, 0x3C)
"Machine = 0x{0:X4}" -f [BitConverter]::ToUInt16($b, $pe + 4)   # 0xAA64 = ARM64
```

---

## 3. Installation on the tablet

1. Copy the contents of `artifacts/driver-ln8000-arm64/` into a separate folder on
   the tablet (for example `C:\nabu-ln8000\`).
2. Enable test signing of drivers and reboot:

   ```powershell
   bcdedit /set testsigning on
   ```

3. Install the driver **as administrator**:

   ```powershell
   cd C:\nabu-ln8000
   .\install-driver.ps1
   ```

   The script checks the signing mode, installs the package, makes sure the device
   `ACPI\QCOM057E` got the `ln8000_kmdf` service, starts it and prints the
   state.

4. Check the mode:

   ```powershell
   .\nabu-ln8000.ps1 status
   ```

Expected: `mode: SWITCHING 2:1` (if the chip confirmed the mode) or
`BYPASS 1:1` (the safe fallback), `critical fault: no`.

---

## 4. Diagnostic tool

| Command | What it does |
|---|---|
| `nabu-ln8000.ps1 status` | mode, session state, `SYS_STS`, faults, telemetry, counters |
| `nabu-ln8000.ps1 sessions` | charge sessions: current and last (duration, peak current, peak temperature, whether the fast mode was on) |
| `nabu-ln8000.ps1 read 1E` | read an LN8000 register directly (hex) |
| `nabu-ln8000.ps1 write 1E 00` | write a register (diagnostics) |
| `nabu-ln8000.ps1 journal out.jsonl` | dump a state snapshot to JSON Lines |

The control codes are computed in the script by the same formula as in
`crates/ln8000-kmdf/src/ioctl.rs`:
`CTL_CODE(0x22, function, METHOD_BUFFERED, FILE_ANY_ACCESS)` ->
`GET_STATUS=0x222040`, `READ_REG=0x222044`, `WRITE_REG=0x222048`,
`SET_LIMITS=0x22204C`, `SET_MODE=0x222050`, `GET_SESSIONS=0x222054`,
`GET_SAMPLES=0x222058`.

Every one of those commands opens the device, so every one of them needs an
**elevated** session: the device object is restricted to `LocalSystem` and
Administrators (`crates/ln8000-kmdf/src/sddl.rs`), and a filtered administrator token
carries the Administrators group for deny-only, which does not match the descriptor. An
unprivileged run fails to open the device. Reading the telemetry marks in
`HKLM\SYSTEM\CurrentControlSet\Enum\...\Parameters` does not need elevation and is the
way to watch the driver from a non-elevated shell.

All seven codes are implemented in the driver:

| Code | Input | Output |
|---|---|---|
| `GET_STATUS` | - | mode, faults, telemetry, counters, last error code |
| `READ_REG` | `addr` | echo of `addr` + `value` of the register read |
| `WRITE_REG` | `addr`, `value` | echo + error code if the write did not go through |
| `SET_LIMITS` | `iin_ua`, `vbat_uv` (0 - do not change) | `applied_iin_ua` - the current **actually** applied (the encoding rounds to a 50 mA step); the limit also reaches the thermal protection |
| `SET_MODE` | `mode`: 1 standby, 2 bypass, 3 switching | `applied_mode` - the mode the chip confirmed |
| `GET_SESSIONS` | - | current and last session with peaks |
| `GET_SAMPLES` | `count` (0 - as many as fit) | samples in a row after the header, `available` - how many are recorded |

> **Caution: `WRITE_REG` writes to the chip directly, past all driver gates.**
> The path is kept for incident analysis (when the charge is already stopped), but it
> checks neither the mode nor Vin: `nabu-ln8000.ps1 write 1E 01` puts
> bit 0 `EN_1TO1` into `SYS_CTRL` and enables the 1:1 bypass **past the Vin gate** - at
> 9-12 V the input voltage ends up on the battery. The internal `Pump::enable_bypass`
> does not do that: it refuses to raise 1:1 at an elevated Vin
> (`ERR_BYPASS_VIN_OUT_OF_WINDOW = -20` for `SET_MODE 2`). After a manual write
> return the chip to a safe state at once - `SET_MODE 1` (standby) or
> `write 1E 00`.

---

## 5. Update and rollback

**Update** (installed on top, the old package is kept in
`%ProgramData%\nabu-fastcharge\backup`):

```powershell
.\update-driver.ps1
```

**Rollback:**

```powershell
.\uninstall-driver.ps1              # stops the service and removes the package
# if needed - restore the saved version:
pnputil /add-driver $env:ProgramData\nabu-fastcharge\backup\ln8000_kmdf.inf /install
```

The driver writes nothing to firmware and does not change power settings, so removal
returns the device to its pre-installation behavior. The removal transcript is
saved to `%ProgramData%\nabu-fastcharge\uninstall.log` - that is the
rollback proof.

---

## 6. Configuration profiles

The defaults are set by the INF and overridden in the device registry:

| Parameter | Default | Meaning |
|---|---|---|
| `IinLimitUa` | 2000000 | input current limit, µA |
| `VbatFloatUv` | 4470000 | charge target voltage, µV (FFC target `qcom,fv-max-uv`; 4.42 V is the QC3 loop limit, not a charge threshold) |
| `VacOvpUv` | 13000000 | input overvoltage threshold, µV (`BUS_OVP_FOR_QC`) |
| `NtcAlarmCfg` | 226 | NTC alarm threshold, 10-bit |
| `TempReduceDc` | 550 | temperature from which the current is reduced (0.1 °C) |
| `TempBypassDc` | 600 | temperature of the switch to bypass (0.1 °C) |
| `TempStopDc` | 650 | charge stop temperature (0.1 °C) |
| `IinMaxUa` | 3500000 | current above which the protection trips, µA |
| `IinTargetUa` | 2000000 | current the protection returns to, µA |
| `IinFloorUa` | 500000 | lower bound of the step-down, µA |
| `VbatReduceUv` | 4450000 | full charge threshold, µV (non-FFC target `qcom,non-fcc-fv-max-uv`; FFC is 4470000) |
| `TelemetryMs` | 250 | telemetry period, ms (100...60000 allowed) |
| `BusRetryCount` | 2 | operation retries on a bus failure (0...8) |
| `WatchdogEnabled` | 0 | enable the chip watchdog timer (0/1) |
| `ProtectionProfile` | 1 | `1` - the pump `V_FLOAT`/`IIN` loops are on, `0` - as in the Device Tree (loops off, only the hardware `VBAT_OV` holds the voltage). Under Windows the pump, not SMB5, drives the battery, so the default is `1` |
| `PublishBattery` | 1 | `1` - the driver attaches the Windows battery class and publishes `GUID_DEVICE_BATTERY` (the Xiaomi miniclass does not, so without this there is no battery meter); `0` - it does not attach it. Set `0` on stacks that bring a battery of their own - a modified-PMIC community pack shows a second battery next to ours. The pump charges either way; the telemetry marks keep flowing, only the Windows-visible battery, `BattPct` and `BattPwr` stop being updated |

**The parameters are read by the driver at device start** - they can be changed without
a rebuild. Key: `<device>\Device Parameters\Parameters`, that is, for example
`HKLM\SYSTEM\CurrentControlSet\Enum\ACPI\QCOM057E\<instance>\Device Parameters\Parameters`.
After an edit, restart the device (disable/enable it in Device Manager or
`pnputil /restart-device`). An invalid value does not corrupt the profile: the driver
rejects it and keeps the previous one - which values were accepted is visible in the
driver debug log and in `nabu-ln8000.ps1 status`.

Protection thresholds computed by the driver itself (the `ln8000::guard` module):

| Threshold | Value | Action |
|---|---|---|
| `temp_reduce_dc` | 550 (55 °C) | reduce the input current to the band setpoint - `min(profile, target)`; with the INF defaults (IinLimitUa 2 000 000 = IinTargetUa 2 000 000) the band coincides with the profile and the step cuts nothing, so the fast mode is set by the registry profile (the code QC profile is 2 800 000, the band then equals the 2 000 000 target); below 520 (52 °C) the current returns to the profile value |
| `temp_bypass_dc` | 600 (60 °C) | switch to bypass 1:1 |
| `temp_stop_dc` | 650 (65 °C) | stop the charge (standby) |
| `iin_max_ua` | 3 500 000 | reduce the current to the 2 A target |
| `vbat_reduce_uv` | 4 470 000 (FFC) | near the top of the charge keep the band setpoint `min(profile, target)`; below the threshold by 50 mV (4.42 V for FFC) the current explicitly returns to the profile value. 4.42 V is the QC3 loop limit, the current is not cut by it |

Protection thresholds are applied with a consistency check: if the resulting set
comes out meaningless (current reduction later than bypass, target above the maximum and
so on), the value is rejected as a whole and the previous thresholds remain - a partially
updated set never happens. Rejected values are visible in the driver debug log.

The strict profile `GuardLimits::conservative()` (no fast mode) -
400/430/500 and current up to 1.5 A: applied when the tablet is used in a warm place.

---

## 7. Typical failures

| Symptom | Cause | What to do |
|---|---|---|
| `failed to open \\.\nabu_ln8000` | the driver is not installed or the device is not started | `install-driver.ps1`, check `Get-PnpDevice` by `ACPI\QCOM057E` |
| The driver did not load, code 52 in Device Manager | test signing is disabled | `bcdedit /set testsigning on` + reboot |
| `mode 2:1 did not engage, staying in bypass` | the chip did not confirm `SYS_STS`, or the protection/PD is not ready | look at `status`: `FAULT1/FAULT2/SAFETY_STS`; check the power supply and the cable |
| `Resource Hub node unavailable` | `_CRS` has no I²C connection (a device without the PEIC node) | cross-check `_HID` in the registry: `HKLM\SYSTEM\CurrentControlSet\Enum\ACPI\QCOM057E` |
| `LN8000 not identified: unexpected device identifier: 0x{got:02X} (expected 0x42)` | a value other than 0x42 was read (the wrong chip, or the bus is not responding) | `nabu-ln8000.ps1 read 00` -> `0x42` is expected |
| The temperature rises, the mode drops | the protection tripped | this is normal; the event is visible in the journal (`actions`), if it repeats - the strict profile |
| Charging does not speed up, mode `BYPASS` | the 9 V negotiation is done by the Type-C/PD part of the platform, not by the pump | see section 9 "Limits" |

---

## 8. Risk register

| Risk | Impact | Rating | What we do |
|---|---|---|---|
| Die overheat in fast mode | damage to the charging node | medium | three-level protection (current reduction -> bypass -> stop), event journal |
| False ADC values (noise, wrong channel) | wrong protection decision | low | decisions are made from three channels, not one; the thresholds have margin |
| No I²C bus response | charging in an unknown mode | medium | timeout 1 s, up to two retries, on failure the device goes to bypass/standby, an event in the journal |
| Negotiation breaks when the supply changes | current surge | low | the driver does not take part in PD negotiation; when power disappears the session is closed |
| Error in the register addresses | chip failure | medium | the addresses are cross-checked with the Android GPL driver, every write is verified by a read |
| Wrong threshold configuration | the fast mode does not engage | low | the profiles are set by constants from the sources, the diagnostics show the actual values |

**Step-by-step rollback:** `uninstall-driver.ps1` -> reboot -> check
`Get-PnpDevice` and `nabu-ln8000.ps1 status` (after removal the tool must not
open the device) -> if needed, restore the previous version from
`%ProgramData%\nabu-fastcharge\backup`.

---

## 9. Limits: what the driver does not do

* **9/11 V negotiation** is done by the Type-C/PD part of the platform (PM8150B), not by the
  charge pump. Without it the LN8000 works in 1:1 bypass from 5 V, or holds the already
  negotiated voltage. The full 22.5-33 W requires separate PD work.
* **The stock charging stack is not replaced**: the driver controls only the charge pump,
  without duplicating `qcbattmngr8150` (per the roadmap this is a race risk).
* **Battery charge control** (profile, JEITA) - stays with the existing battery stack.

---

## 10. Acceptance test protocol

The run is automated: **one command** walks the items, captures the driver
telemetry, collects the meter readings and produces the final protocol.

```powershell
cd C:\nabu-ln8000
.\run-acceptance.ps1                      # interactive: the script asks for the multimeter readings
.\run-acceptance.ps1 -MeterJson .\meter.json   # non-interactive, readings from a file
.\run-acceptance.ps1 -DryRun              # pipeline check without hardware
```

The output lands in `%ProgramData%\nabu-fastcharge\acceptance\`:
`acceptance-<timestamp>.html` (protocol), `.md` (markup), `.json`
(machine-readable summary) and `journal-<timestamp>.jsonl` (the attached session journal).
The report counts the base and the fast modes, the power gain in percent, the
die temperature and the driver mode at the moments of measurement.

Format of `meter.json`:

```json
{ "base_volt": 5.05, "base_amp": 1.80,
  "fast_volt": 9.02, "fast_amp": 2.95,
  "hold_volt": 9.01, "hold_amp": 2.90, "hold_minutes": 10 }
```

The order and the criteria that the script checks:

| No | Scenario | How we measure | Acceptance criterion |
|---|---|---|---|
| 1 | Base mode, 5 V | external USB multimeter | record the current/voltage, power as the baseline |
| 2 | Fast mode | multimeter + `nabu-ln8000.ps1 status` | mode `SWITCHING`, power gain relative to row 1 |
| 3 | Mode hold | 10 minutes in a row | the mode does not roll back to standby, the temperature is below `temp_reduce_dc` |
| 4 | Thermal protection | heating (or the strict profile) | an event in the journal, current reduced/bypass, the charge is not cut off abruptly |
| 5 | Swap to an incompatible supply | switching the supplies | the session closes, a new session opens, no hangs |
| 6 | Cable unplugged during negotiation | unplug/plug the cable | the device returns to a working state, an event in the journal |
| 7 | Reboot | `Restart-Computer` | after boot the driver comes up, the mode is restored |
| 8 | Clean installation | on a device without the driver | `install-driver.ps1` without errors, the mode is available without registry edits |
| 9 | Rollback | `uninstall-driver.ps1` | the device returns to the stock behavior, no leftover services |
| 10 | Repeat installation | `install-driver.ps1` | completes successfully, the session counters start over |

Results for each item: the measured value + the output of `nabu-ln8000.ps1
journal` to a file. The summary is collected in `docs/ACCEPTANCE.md`.

### What has already been checked without hardware

A run on the I²C mock (in the LN8000 core) confirms all the logic except the physical
connection: probe `DEVICE_ID = 0x42`, threshold configuration, enabling 2:1 mode
and its verification, fault parsing, ADC reads, protection tripping. See
`artifacts/verify-pump.txt` and the 43 tests of `cargo test -p ln8000`.

The report pipeline was verified by a dry run (`-DryRun`): it produces HTML,
Markdown and JSON, computes the power gain and marks the file as not a real measurement.
Example - `artifacts/acceptance-dryrun/`.
