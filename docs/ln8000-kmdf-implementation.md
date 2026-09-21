# LN8000: KMDF implementation (Rust)

Applied steps for `crates/kmdf` and the future SPB transport to the LN8000.

General context: `01-analysis/ln8000-windows-driver-roadmap.md`.

---

## Architecture

```
[Battery miniclass / CLI]  ←→  [KMDF LN8000]  ←→  SpbCx / I²C (QUP)
                                      ↑
                               ln8000_charger.c (logic)
                               core crate (policy, if needed)
```

The current `kmdf` crate is a WDK skeleton; the SPMI/SPB transport is under development (`crates/kmdf/src/spmi.rs`).

---

## Step 1 - SPB / I²C

1. INF: `HardwareIds` = `ACPI\QCOM057E` (the existing PEIC) or a custom `LNX8000`.
2. `EVT_WDF_DEVICE_PREPARE_HARDWARE`: parse `_CRS`, `SpbTargetDeviceConnect`.
3. Probe: `SpbRead` of register `0x00` -> `DEVICE_ID == 0x42`.

Samples: `08-driver-samples/Windows-driver-samples/spb/`.

---

## Step 2 - Init (from DTS)

The thresholds and disable flags are in the table in `04-android-reference-sources/LN8000.md`.

The minimal set of writes after probe:

- `THRESHOLD_CTRL`, `NTC_CTRL`
- `FAULT_CTRL` (account for the protections disabled in the DT)
- `REGULATION_CTRL`, `IIN_CTRL`, `V_FLOAT_CTRL`

---

## Step 3 - IRQ

- Register `WdfInterruptCreate` on the GPIO from `_CRS` (GPIO **36** on nabu).
- In the DPC: read `INT1`, `SYS_STS`, `FAULT1/2_STS`, `SAFETY_STS`.
- Masks: `INT1_MSK`.

---

## Step 4 - op_mode

The target state: **`LN8000_OPMODE_SWITCHING` (3)**.

Control through `SYS_CTRL` (`STANDBY_EN`, `EN_1TO1`), `CHARGE_CTRL`, `REGULATION_CTRL`.

**Acceptance test:** after the charger is enabled `op_mode == 3` is stable; a fall back to `1` (STANDBY) = an init/PD/protection bug.

---

## Step 5 - IOCTL / integration

Options:

- a separate control device + IOCTL (as in `crates/kmdf/src/ioctl.rs`);
- a link to the battery miniclass through a shared interface / WMI.

Do not duplicate `qcbattmngr8150` without coordination.

---

## Step 6 - build and debug

```text
# from 11-driver-rust/crates/kmdf (a separate workspace, ARM64)
cargo wdk build --target aarch64-pc-windows-msvc
```

- Test signing, Secure Boot off.
- WinDbg: `!wdfkd.wdfdevice`, SPB trace.
- The sequence: **DEVICE_ID -> thresholds -> op_mode 3 -> IRQ**.

---

## Dependencies outside the LN8000

| Block | Status under Windows |
|---|---|
| Fuel gauge PM8150 | ✅ |
| SMB5 / APSD | ❌ (a separate track, `core` crate) |
| PM8150B PD / Type-C | ❌ (needed for 9 V+) |

---

## Links

- Registers: `04-android-reference-sources/drivers_power_supply_ti_ln8000_charger.h`
- Logic: `04-android-reference-sources/drivers_power_supply_ti_ln8000_charger.c`
- ACPI: `09-acpi-nabu/ln8000-acpi-uefi.md`
