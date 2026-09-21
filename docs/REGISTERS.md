# Register map

All addresses are in the USBIN peripheral space of the SMB charger (PM8150B).
Source: the reference Android driver, `drivers/power/supply/qcom/smb5-reg.h`
and `smb-reg.h` (branch 16.0, configuration `CONFIG_MACH_XIAOMI_NABU`).

## Registers the driver works with

| Address | Name | Purpose |
|---|---|---|
| `0x1307` | `APSD_STATUS` | state of the detection state machine: bit 0 - "detection complete", bit 1 - "Quick Charge connected", bit 5 - slow plug-in, bit 6 - HVDCP check timeout |
| `0x1308` | `APSD_RESULT_STATUS` | detection result: adapter type in bits 6:0 |
| `0x1309` | `QC_CHANGE_STATUS` | state of the Quick Charge negotiation |
| `0x1340` | `USBIN_CMD_IL` | input control, bit 0 - suspend |
| `0x1341` | `CMD_APSD` | command to the detection state machine, bit 0 - rerun (`APSD_RERUN`) |
| `0x1342` | `CMD_ICL_OVERRIDE` | forced input current limit: bit 0 - enable, bit 4 - apply after APSD |
| `0x1343` | `CMD_HVDCP_2` | HVDCP2 mode control |
| `0x1344` | `USBIN_ADAPTER_ALLOW_OVERRIDE` | override of the allowed adapter types |
| `0x1345` | `USB_CMD_PULLDOWN` | D+/D- pulldowns |
| `0x135B` | `HVDCP_PULSE_COUNT_MAX` | QC2 voltage: bits 7:6 (`00` = 5 V, `40` = 9 V, `80` = 12 V) |
| `0x1366` | `USBIN_ICL_OPTIONS` | extra input current limit options |
| `0x1370` | `USBIN_CURRENT_LIMIT_CFG` | input current limit code |

## Detection result patterns

Pattern (the value of `APSD_RESULT_STATUS & 0x7F`) -> adapter type. The table follows
`smblib_apsd_results[]`; bits: `SDP = BIT(0)`, `OCP = BIT(1)`, `CDP = BIT(2)`,
`DCP = BIT(3)`, `FLOAT = BIT(4)`, `QC_2P0 = BIT(5)`, `QC_3P0 = BIT(6)`.

| Pattern | Type | Current limit | QC2 | Pump eligible |
|---|---|---|---|---|
| `0x00` | UNKNOWN | 100 000 µA | - | no |
| `0x01` | SDP | 500 000 µA | - | no |
| `0x02` | OCP | 100 000 µA | - | no |
| `0x04` | CDP | 1 500 000 µA | - | no |
| `0x08` | DCP | 1 500 000 µA | - | no |
| `0x10` | FLOAT | 100 000 µA | - | no |
| `0x28` | HVDCP2 (`DCP + QC_2P0`) | 1 500 000 µA | 9 V | no |
| `0x48` | HVDCP3 (`DCP + QC_3P0`) | 3 000 000 µA | 9 V | yes |
| `0x48` + QC3.5 authentication | HVDCP3P5 | 3 000 000 µA | 9 V | yes |

The type refinement logic (from `smblib_get_apsd_result`): if the `QC_CHARGER` bit is
set, then `HVDCP3` stays `HVDCP3` (or becomes `HVDCP3P5` after the authentication is
confirmed), and everything else is treated as `HVDCP2`.

## Current limit grid

The register value is the step number: `current = 100 mA + step x 100 mA`,
step 0...31. Constants from Android: `DCIN_ICL_MIN_UA = 100000`,
`DCIN_ICL_STEP_UA = 100000`.

Currents by type (from `smb5-lib.h` / `cp_qc30.h`):

| Constant | Value | Meaning |
|---|---|---|
| `HVDCP_CURRENT_UA` | 3 000 000 | HVDCP3 |
| `HVDCP2_CURRENT_UA` | 1 500 000 | HVDCP2 |
| `HVDCP_CLASS_A_MAX_UA` | 2 500 000 | class A limit |
| `HVDCP_CLASS_B_CURRENT_UA` | 3 100 000 | class B limit |
| `HVDCP3P5_40W_CURRENT_UA` | 4 500 000 | the stock adapter through the charge pump |
| `DCIN_ICL_MAX_UA` | 1 500 000 | input limit |

## What remains to be clarified

* The layout of the SPMI bus response for a register read
  (`IOCTL_RESOURCE_HUB_TRANSACT = 0x32C004`, confirmed by reverse engineering of
  `qcspmi8150.sys`): which bytes carry the register value.
* The LN8000 charge pump registers (I2C 0x51) - a separate stage, the protocol is in
  the Android sources (`ln8000_charger.c`).
