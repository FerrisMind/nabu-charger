# Compatibility matrix

What has been checked, and on what. Three independent layers are separated: **the device**,
**the platform (Windows/UEFI)** and **the power supply (brick)** - the fast mode
depends on the combination of all three.

## 1. Device

| Device | Code | SoC | PEIC node | I²C | Status |
|---|---|---|---|---|---|
| Xiaomi Pad 5 | nabu | SM8150 | `QCOM057E`, `_STA=0x0F` | 0x51 on `\_SB.I2C5` | primary; the driver is built and configured |
| Other SM8150 (for example Mi 9) | - | SM8150 | not confirmed | - | **not tested**; the driver will bind if the node exists and the address is 0x51 |
| Devices with an LN8000 but without the PEIC node in the DSDT | - | - | no | - | not supported: an ACPI change is needed (outside the scope of work) |

## 2. Platform

| Component | Version | Status |
|---|---|---|
| Windows | 11 25H2 ARM64, build 26200.9445, ru-RU | installed, working |
| Driver signing | test (`testsigning on`), Secure Boot off | confirmed |
| Qualcomm driver package | 2608.03 (120 INF) | installed |
| WDK | 10.0.26100 | the build passes |
| Rust | 1.97.0 | the build passes |
| LLVM/libclang | **17.0.6** (required: 23.x breaks bindgen) | confirmed |
| cargo-wdk | 0.5.x | `--target-arch arm64` |

## 3. Power supplies

| Supply type | What it provides | LN8000 fast mode | Status |
|---|---|---|---|
| PD (Xiaomi T2510, Honor) | AC, 5 V, the negotiation above 5 V is on the PD controller side | possible if the PD source delivered >=9 V | the PD supply provides AC, but **the capacity does not grow** - being investigated separately |
| USB-A with HVDCP/QC | in practice SDP/DCP, no fast negotiation | no: there is no elevated voltage | recorded in the measurements |
| A plain PC USB-A port | SDP, 500 mA | no | expected |

## 4. Software combinations

| Scenario | Expectation | Verified |
|---|---|---|
| Our LN8000 driver + the stock battery stack | the driver controls only the pump, the stack controls the battery | logically: the domains are separated; on hardware: no |
| Our driver + the SMB detection driver | independent: different nodes (SMB through Resource Hub/SPMI, LN8000 through I²C) | both build, the installation is independent |
| No driver | the previous behavior: charging without the fast mode | verifiable by rollback |

## 5. What remains untested and why

| Item | Reason |
|---|---|
| Real power and current in fast mode | an external USB multimeter and a tablet with the driver are needed |
| Behavior on reboot and rollback on hardware | access to the tablet in Windows is needed |
| Compatibility with supplies that deliver exactly 9 V without PD | a set of supplies is needed |
| SM8150 devices other than nabu | the corresponding hardware is needed |

These items are listed in the acceptance protocol (`docs/DEPLOY-LN8000.md`, section 10)
and in `docs/ACCEPTANCE.md` with the status "a test bench is needed" - no invented results.

---

## 6. D+/D- negotiation protocols (QC / AFC) - state as of 17.09.2026

> *Correction 18.09.2026: the row about AFC and the byte `0x28` is fixed, the conclusions about the
> SPMI path being unavailable were disproved by live measurements - see section 7.*

Checked on a live device and against the sources. The causes are separated:

| Cause | Evidence | Outcome |
|---|---|---|
| Windows has no D+/D- detection and no input voltage control | a scan of strings and IOCTLs across the Qualcomm 2608.03 package: no `APSD`, no `QC2/QC3`, no `DCP`, no `charger_type` | QC is not negotiated |
| The HVDCP block in the PM8150B is not enabled | the reference enables it by writing `USBIN_OPTIONS_1_CFG (0x1362)` = `HVDCP_EN|AUTH_ALG_EN` and clearing the autonomous mode (`smb5-lib.c:1324-1346`); Windows has no such writes | VBUS is not raised |
| AFC is not supported by the hardware | zero `afc` matches across the nabu device tree, the kernel and `hvdcp_opti` (`04-android-reference-sources`, `16-firmware-extract`); Windows mark `SuAfcProto=0` | TA220 (AFC): only a plain DCP 5 V source with a higher current; AFC negotiation is not performed |
| The battery manager clears the HVDCP flag without the registry currents | Ghidra `FUN_140006148`: without `PropChargers` the HVDCP bit is cleared | the limits were not applied |

### 6.1 Measurements (17.09.2026)

| Source | VBUS | Pump mode | Input current | Battery | Die |
|---|---|---|---|---|---|
| PD supply | 8.86-8.96 V | 2:1 (switching) | 1.65-1.91 A | 4.03-4.06 V | 33-40 °C |
| 5 V supply | 4.80-4.82 V | standby | 39 mA | 4.13 V | 33 °C |

Power at the pump input with a 9 V source: 1.91 A x 8.96 V ~ 17.1 W (to the battery in 2:1 ~ 15 W).
With a 5 V source the pump cannot work (8.2 V or more is needed) - the charge comes only from the main charger.

### 6.2 Applied configuration and rollback

Written into the battery manager device key, the battery miniclass key and the service key:
`PropChargers\\HVDCPChargerCurrent = 3000000`, `HVDCPV3ChargerCurrent = 3000000`, `IWallChargerCurrent = 3000000`,
`ParallelCharging\\FeatureEnable = 1`. Date: 17.09.2026, a reboot was performed after the write.

The rollback was verified on the device: the keys were removed, their absence was confirmed, then they were restored.
Script: `14-hvdcp/03-apply-hvdcp.ps1 -Remove`.

### 6.3 Register map for a QC implementation (from the reference)

Base `USBIN_BASE = 0x1300`. Details and the limiting factors are in the report
`14-hvdcp/negotiation-protocols-report-2026-09-17.html` and the analysis `14-hvdcp/research/subagent_01_reference-mechanics.md`.

| Register | Address | Bits | Purpose |
|---|---|---|---|
| `USBIN_OPTIONS_1_CFG` | `0x1362` | `BIT2 HVDCP_EN`, `BIT6 AUTH_ALG`, `BIT3 BC1P2_SRC_DETECT`, `BIT5 AUTONOMOUS`->0 | enable detection and negotiation |
| `APSD_STATUS` | `0x1307` | `BIT0 DTC_DONE` | detection complete |
| `APSD_RESULT_STATUS` | `0x1308` | the low 7 bits | source type (DCP `0x08`, HVDCP2 = QC 2.0 `0x28` = `DCP_CHARGER_BIT\|QC_2P0_BIT`, HVDCP_3 `0x48`) |
| `CMD_APSD` | `0x1341` | `BIT0 RERUN` | restart the detection |
| `CMD_HVDCP_2` | `0x1343` | `BIT0`/`BIT1` - QC3.0 pulses; `BIT3/4/5` - force 5/9/12 V | voltage step-up |

### 6.4 What remains

Access to the PM8150B registers over SPMI from Windows. The SPB sequence code and the path through `\\Device\\RESOURCE_HUB`
exist in the project (`crates/spb`, `crates/kmdf`), but are not verified on hardware: the stock clients perform a
connection step (`0x32C004`) before working, which we do not have yet. That is the next step of work.

### 6.5 Additional facts (17.09.2026, after the reboot)

| Fact | Value / evidence |
|---|---|
| The `HVDCP` flag in the USB function | `HKLM\SYSTEM\CurrentControlSet\Services\QcUsbFnSs\Parameters\HVDCP` was `0`, set to `1` |
| Type-C port re-initialization | `pnputil /restart-device` for `ACPI\QCOM057D` and `ACPI\QCOM0582` - applies after a reboot |
| Result | after the reboot the input stayed at `4.816 V`: no step-up, configuration cannot fix this |
| Path to the pump | `PumpOpen=1`, `PumpBusOpen=1`, `HubValue=0x42` - the I²C exchange is alive |
| Path to the PMIC registers | `AttachStatus=0xC0000034` (object not found), `HubS3/S4/S5=0xC000000D` - the connection to the hub is not performed |
| Battery miniclass ETW | in 40 s only 5 service records, no charging events |
| UCSI channel | `Microsoft-Windows-USB-UCMUCSICX/Operational` is empty |
| Earlier measurements under Windows | the original Xiaomi supply (USB-A, HVDCP3): discharge, -134 mW*h in 6 min, `BatteryStatus=1`; Samsung EP-T2510 (PD 25 W): 0 % in 1800 s; Honor: +1 % in 60 s |
| The first telemetry under Windows | only from the pump ADC through our driver; `ChargeRate`/`DischargeRate` and `root\wmi BatteryStatus` are unavailable on this build |

### 6.6 Resource node connection enumeration (17.09.2026, driver on the device)

The node's own connection (identifier 1) opens and reads the pump: `HubValue = 0x42`.
Other identifiers do not open: `Sc2Open`/`Sc3Open`/`Sc4Open`/`Sc5Open`/`Sc6Open`/`Sc8Open`/`Sc16Open`/`Sc31Open` = 0.
The connection command `0x32C004` is rejected on both routes: `AttachStatus` and `ParAttachStatus` = `0xC0000034`.

Conclusion: access to the PM8150B registers from our node is closed by the resource node access rule, not by a setting.
A QC implementation requires an ACPI/UEFI change or a driver bound to a node with a declared SPMI connection -
both are outside the current constraint of "not rewriting ACPI".

> *Correction 18.09.2026: the conclusion about unavailability was disproved - on build 20.47.10.605 the object
> `\Device\Spmi\SUPERUSER` opened and QC3 negotiation went through it, see section 7.*

### 6.7 Correction to the percentage measurements

- `GetSystemPowerStatus` returns the percentage `255` (unknown), and the battery report returns zero capacity.
- So the earlier conclusions "0 % in a minute" and "discharge" rested on an unreliable signal.
- Only the pump ADC readings and the battery voltage gain are reliable.
- With a 5 V source the battery grows: 4140 -> 4175 mV in 17 min 48 s, the pump in standby (39 mA), 32-34 °C.
- The driver rejects the pump pass-through mode (code -4): only standby and the 2:1 mode are available.

### 6.8 1:1 pass-through mode and the valid input flag (17.09.2026)

The reference input check: `sys_st == 0x02 && fault1_st == 0x00` = "valid VBUS" (`ln8000_charger.c:1311-1320`).
Measured with a 4.816 V source: `SYS_STS = 0x22`, `FAULT1_STS = 0x21` - the voltage bits are set, the input is invalid.
Writes do reach the chip: `IIN_CTRL (0x1B)` = `0x20` -> back `0x20`, then `0x38` -> `0x38`.
A direct write `SYS_CTRL (0x1E) = 0x01` was accepted by the register, but the pass-through mode did not come up (bit 3 in `SYS_STS` is clear).

Driver change: the charge start (`set_charging`) follows the reference logic - 2:1 first, 1:1 on failure,
and if both fail the chip is put into standby. The build was installed on the tablet (package `oem137.inf`), the device is OK.

Tools: `nabu-probe` - the commands `dump <from> <to>` and `wreg <address> <value>`.
The matrix is collected automatically: `14-hvdcp\40-adapter-matrix.ps1` plus the ordering file `data\adapter-order.txt`.

### 6.9 Automatic charge start in the driver (17.09.2026)

The driver timer starts the charge by itself when the input is 4.6 V or more: the order 2:1 -> 1:1 -> standby, a retry no more often than every 30 s
and immediately when the input changes by 0.3 V. Previously the charge was enabled only by the `SET_CHARGE` command.
Verified: build `oem138.inf`, the SHA-256 matches the local package (`EE6D1093...`); after the chip failure
`SYS_CTRL = 0x08`, `SYS_STS = 0x02` - a safe state. Exchange counters: writes 49 -> 137, reads 152 -> 529 in 95 s.

### 6.10 Persistence across a reboot and the ACPI path (17.09.2026, 14:05-14:12)

Boot at 14:05:05 with the final build: services Running, device OK, driver version 13.59.12.861,
package `ln8000_kmdf.inf_arm64_762a54bc449a5217`. After the bring-up `SYS_CTRL = 0x08`, `SYS_STS = 0x02` -
the driver ran the autostart and left the chip in a safe state; telemetry is flowing (39 mA, 4816 mV, 32.6 °C).

ACPI: the `_DSM` of the `\_SB.URS0.USB0` node - only the standard USB controller capabilities, there is no current
control interface. SPMI connections are declared only on `ADC1`/`ADC2`/`ADC3`; there is no connection to the charger
registers in ACPI - so Quick Charge is unreachable from Windows without an ACPI/UEFI change.

> *Correction 18.09.2026: the "unreachable" conclusion was disproved - the `\Device\Spmi\SUPERUSER` path worked on
> build 20.47.10.605, see section 7.*

### 6.11 The latched fault is cleared: fast charging from 5 V (17.09.2026, 14:09-14:16)

The stock reset from the reference (`LION_CTRL = 0xC6`, then bit 0 in `BC_OP_2`, a 10 ms pause) cleared `FAULT1` (0x21 -> 0x00),
after which the 1:1 mode engaged: `SYS_STS = 0x28`. Measured: input 4.40-4.45 V, current 2826-2904 mA,
battery 4280 -> 4330 mV in 6 minutes (~9.5 mV/min), die 38.2-39.1 °C, about 12.6 W at the input.

The driver (build `oem139.inf`) got self-recovery: after three failed charge attempts in a row - `soft_reset()`
and `configure()`, then a retry. Verified after deployment: the mode holds by itself.

---

## 7. Corrections 18.09.2026 (live measurements)

- **The SPMI path works.** On driver build 20.47.10.605 the object `\Device\Spmi\SUPERUSER` opened
  (`SpmiProbeSuperuser = 0`, `STATUS_SUCCESS`), negotiation went through it (`HvdcpVia = 1`) and completed a
  full APSD + QC3 cycle: `ApsdSt = 0x4F` (`DONE|DCP|QC3`), `ApsdResult = 0x48` (`DCP|QC3`),
  `PulseCnt = 20`, `HvdcpPhase = 6` (Done), VBUS 9.5-9.9 V. Evidence: `19-live/evidence-dump.txt`,
  the record `qc35-post_2026-09-18_001305`; a live read on 18.09.2026 12:35 (`SpmiProbeSuperuser = 0`,
  `19-live/11-marks-out.txt`). The earlier unavailability conclusion referred to an older build.
- **The APSD byte `0x28` is HVDCP2 (QC 2.0), not "AFC-like 5 V".** Per the reference
  (`04-android-reference-sources/drivers_power_supply_qcom_smb5-lib.c`, around line 565)
  `HVDCP2.bit = DCP_CHARGER_BIT | QC_2P0_BIT = 0x08 | 0x20 = 0x28`; `QC_2P0_BIT = BIT(5)`
  (`drivers_power_supply_qcom_smb5-reg.h:222`), `DCP_CHARGER_BIT = BIT(3)`. The previous logic
  (`apsd_is_afc_like_5v`) sent such supplies into a 5 V bypass without `FORCE_9V` - that rejected real
  QC 2.0 supplies; fixed on 18.09.2026 (the QC2 `FORCE_9V` branch, bypass - only a fallback when the voltage is not raised).
- **AFC is absent on nabu** (see the fixed row in section 6): the Samsung TA220 is served
  as a plain DCP 5 V source with a higher current, without AFC negotiation.
- **The full charge setpoint is 4.45 V** (registry, `VbatFloatUv`), the `V_FLOAT_CTRL` register limit is 4.47 V
  (`0x95` = 149). 4.42 V (`mi,qc3-bat-volt-max`) is a ceiling of the QC3 mode only. Encoding:
  code = (mV - 3725) / 5 (4450 mV = `0x91`, 4420 mV = `0x8B`, 4470 mV = `0x95`). With a 4.42 V target the pump
  hit its own CV limit at a battery of 4.39-4.49 V, the `VFLOAT_LOOP` loop tripped (bit 6 of `SYS_STS`),
  and the current dropped to 39 mA.
