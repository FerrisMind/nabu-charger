# HVDCP / SPMI: next probe (2026-09-17)

## Breakthrough?

**Maybe** — not yet proven on device. Direct `\Device\Spmi\SUPERUSER` is a dead end
(all access masks → `0xC0000001`). The remaining high-probability path is **not** a
private SpmiArb protocol; it is a **connection-bound Resource Hub client** that already
talks SPMI to PM8150B.

## Deliverables (reverse-engineering verdict)

### 1. Exact connection ID / ACPI path for USBIN SPMI

**Absent from stock nabu DSDT.**

| What exists | Path / encoding |
|---|---|
| SPMI controller | `\_SB.SPMI` (`ACPI\QCOM050C`) — MMIO only, no client grant |
| ADC SPMI clients | `\_SB.ADC1/2/3` (`ACPI\QCOM0512`, UID 0/1/2) |
| ADC2 (SID **2**) | SerialBus `0xC1`, SID=`0x02`, peripherals **`0x0131`** (VADC) and **`0x0135`** (ADC_TM) — **not** USBIN `0x13` |
| PEIC | `\_SB.PEIC` (`ACPI\QCOM057E`) — **I²C5@0x51 only** |

There is **no** ACPI SerialBus descriptor with peripheral `0x0013` (USBIN base
`0x1300`). Therefore Resource Hub never assigns a ConnectionId for USBIN, and no
third-party KMDF can open `\Device\RESOURCE_HUB\<id>` for `0x1343` on stock firmware.

ConnectionId ≠ peripheral ID. RH `Id` is assigned at translate time to the device
that owns the `_CRS` entry; probing hub paths `0x131`/`0x135`/`0x13` from PEIC is
diagnostic only (almost always `Open=0`).

### 2. Can PEIC gain SPMI via ACPI overlay?

**Yes — required.** Add one SPMI SerialBus descriptor (SID=2, periph=`0x13`) to
PEIC `_CRS` (keep existing I²C).

Exact template: [`09-acpi-nabu/ssdt-peic-usbin-spmi.asl`](../../09-acpi-nabu/ssdt-peic-usbin-spmi.asl)

```
0x8E 0x13 0x00 0x01 0x00 0xC1 0x02 0x02 0x13 0x00 0x00 0x00 + "\_SB.SPMI"
```

(Same shape as ADC2’s VUSR/VBTM, SID byte `0x02`, peripheral LE `0x0013`.)

Nodes:

1. Keep `\_SB.PEIC` (`QCOM057E`) — override `_CRS` via SSDT (UEFI / Aloha).
2. Depend on `\_SB.SPMI` (already present).
3. **Do not** expect Windows registry SSDT injection on production nabu.

After overlay: `ConnCount=2`, `UsbinConn=1`, driver probes `APSD_STATUS` (`0x1307`).

### 3. IOCTL on QCOM_ADC / QCOMPMIC for register write?

**No usable public register-write IOCTL found.**

| Device | Symlink | Strings / INF | Register write? |
|---|---|---|---|
| `qcadc8150` | `\??\QCOM_ADC`, `QCOM_ADC2`, `QCOM_ADC3` | creates links; ACL Admin/SYSTEM; **no** IOCTL name strings | ADC sample path only (not USBIN) |
| `qcpmic8150` | `\DosDevices\Global\QCOMPMIC` | opens `\Device\Spmi\SUPERUSER` internally; PON/WDOG strings | private; no CTL_CODE table in strings |
| Charging stack | `QCOMBATTMGR`, etc. | no HVDCP/USBIN IOCTL names in package | — |

Staff path for PMIC regs: open **own** RH connection → `IOCTL_SPB_EXECUTE_SEQUENCE`
(`0x41808`). Attach helper `0x32C004` is RH connection setup, not a byte poke API.
`qcpep8150` exposes GPIO/OTG/LPG IOCTLs only — not USBIN/`CMD_HVDCP_2`.

Opening `QCOM_ADC*` from PEIC (if ACL allows) does **not** steal ADC2’s SID2
connection; those are separate device objects.

## What failed

| Probe | Result |
|---|---|
| User-mode `\\.\Spmi\SUPERUSER` | path not found (no DosDevices link) |
| Kernel `\Device\SpmiArb` | `STATUS_OBJECT_NAME_NOT_FOUND` (`0xC0000034`) |
| Kernel `\Device\Spmi`, `\Device\Spmi\SUPERUSER` (8 masks, ShareAccess=0) | `STATUS_UNSUCCESSFUL` (`0xC0000001`) |
| Registry `HVDCP=1` / `PropChargers\*Current` | VBUS stays ~4.8 V — no D+/D− negotiation |
| PEIC `_CRS` hub IDs (own I2C only) | no SPMI SID2 / USBIN `0x13xx` |

## Exact technical approach

Physics of QC is a **byte write** to PM8150B SID **2**:

1. `0x1362` — enable `HVDCP_EN` + `HVDCP_AUTH_ALG_EN` (+ BC1.2)
2. `0x1341` — `APSD_RERUN`
3. `0x1343` (`CMD_HVDCP_2`) — `SINGLE_INCREMENT` / `SINGLE_DECREMENT` / `FORCE_5V`

Windows already has a working SPMI client stack: `qcspmi8150.sys` +
`IOCTL_SPB_EXECUTE_SEQUENCE` (`0x41808`) via `\Device\RESOURCE_HUB\<id>`.
Proof: `qcadc8150.sys` on `ACPI\QCOM0512\0/1/2` (Started) with SPMI `_CRS` to SID 0/2/4
**ADC peripherals only**.

**Blocker:** PEIC (`QCOM057E`) only has I²C5/0x51. Hub hands out **only the caller’s**
connection IDs. No USBIN connection → no `0x1343`.

## Implemented in ln8000-kmdf (this pass)

1. Named opens with `FILE_SHARE_*`: `QCOMPMIC` / `QCOM_ADC*` / `RESOURCE_HUB` / `SpmiSuShare`.
2. Hub ID candidates include `0x13` (still ≠ ConnectionId; diagnostic).
3. **Dual-connection**: `select_i2c_connection` + `select_usbin_connection`.
4. **`SpbBus::transact_spmi16`** — 16-bit SPMI R/W (BE and LE probe of `0x1307`).
5. Marks: `UsbinConn`, `UsbinLow/High`, `UsbinOpen`, `UsbinBeSt/Val`, `UsbinLeSt/Val`.
6. ASL overlay template: `09-acpi-nabu/ssdt-peic-usbin-spmi.asl`.

### WS-C HVDCP state machine (gated)

New module `crates/ln8000-kmdf/src/hvdcp.rs` + `IOCTL_LN8000_RUN_HVDCP` (`0x818`):

| Step | Reg | Action | Mark |
|---|---|---|---|
| Gate | — | `usbin_id == None` → `HvdcpErr=-10`, **no SPMI** | `HvdcpAuto=0` on stock |
| Enable | `0x1362` | set `HVDCP_EN`+`AUTH_ALG`+`BC1P2`, clear autonomous | `HvdcpEnSt` |
| APSD | `0x1341` / `0x1307`/`0x1308` | rerun, wait DONE, read result | `ApsdSt`, `ApsdResult` |
| QC3 | `0x1343` INC | soft `PulseCnt`, 200 mV steps → `2*VBAT+200mV` (max 30) | `PulseCnt`, `HvdcpTarget` |
| QC2 | `0x1343` FORCE_9V | force 9 V path | `HvdcpPhase=4` |

Autostart runs once after LN8000 pump open **only** when `UsbinConn` id is present.
Stock ACPI: `select_usbin_connection` → `None` → autostart marks and returns; I2C LN8000 path unchanged.

### How to read on device

```powershell
$dev = 'ACPI\QCOM057E\2&DABA3FF&0'
$m = Get-ItemProperty ("HKLM:\SYSTEM\CurrentControlSet\Enum\$dev\Device Parameters")
$m.PSObject.Properties |
  Where-Object { $_.Name -match '^(PmicOpen|AdcOpen|SpmiSu|HubOpen|Usbin|Sc131|Sc135|Sc13|Sc56|ConnCount|C0)' } |
  Sort-Object Name |
  ForEach-Object { '{0,-22} = 0x{1:X8}' -f $_.Name, [uint32]$_.Value }
```

Interpret: `0` = opened / SPB ok; `C0000034` = missing; `C0000022` = access denied;
`C0000001` = rejected; `UsbinConn=0` = stock ACPI (expected until SSDT).

## Concrete next on-device experiment

**Order matters — do A before B.**

### A. Deploy current driver (no firmware change)

1. Install rebuilt `ln8000-kmdf`, restart `ACPI\QCOM057E\*`.
2. Read marks above.
3. Expect: `UsbinConn=0`, `UsbinOpen=0xFFFFFFFF`; record whether any
   `AdcOpenQcomAdc*=0` or `PmicOpenQcompmic=0` (ACL probe only — still no reg write).
4. If an `AdcOpen*` is `0`: note it; do **not** brute-force IOCTLs yet (no public
   write surface). Prefer overlay path.

### B. Firmware SSDT (unblocks USBIN)

1. Build/flash UEFI with `ssdt-peic-usbin-spmi.asl` (or merge `_CRS` into DSDT).
2. Reboot; confirm PEIC still Starts with ln8000.
3. Expect: `ConnCount=2`, `UsbinConn=1`, `UsbinOpen=1`.
4. Success criterion: **`UsbinBeSt=0` or `UsbinLeSt=0`** and `Usbin*Val` is a
   plausible APSD byte (often bit0 set when VBUS present).
5. Next code step after success: ~~write sequence~~ **done (WS-C)** —
   `IOCTL_LN8000_RUN_HVDCP` / autostart (`hvdcp.rs`); confirm `HvdcpPhase=6`,
   `PulseCnt` / `ApsdResult`, and VBUS rise on a QC adapter.

### C. If SSDT impossible short-term

Temporary bind/upper-filter on `ACPI\QCOM0512` (ADC2, SID2) only proves SPMI SPB
to **0x31xx/0x35xx**, not USBIN `0x13xx` — low value for HVDCP. Prefer B.

## Files touched

- `crates/ln8000-kmdf/src/spb.rs` — `transact_spmi16` / `prepare_spmi16`, share probes
- `crates/ln8000-kmdf/src/lib.rs` — dual connection + USBIN APSD probe + hub `0x13` + HVDCP hooks
- `crates/ln8000-kmdf/src/hvdcp.rs` — gated HVDCP/QC state machine (WS-C)
- `crates/ln8000-kmdf/src/ioctl.rs` — `IOCTL_LN8000_RUN_HVDCP`
- `09-acpi-nabu/ssdt-peic-usbin-spmi.asl` — PEIC `_CRS` overlay template
- this note
