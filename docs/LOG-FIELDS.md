# Reference of journal and telemetry fields

Three layers of data, from fast to long-lived:

1. **Samples** (`TelemetrySample`) - a ring in driver memory, up to 256 records.
2. **Sessions** (`ChargeSession`) - history in driver memory, up to 32 sessions.
3. **The journal on disk** (JSON Lines) - what survives a reboot.

---

## 1. Telemetry sample

Source: `crates/ln8000/src/session.rs`, structure `TelemetrySample`.

| Field | Type | Unit | Where it comes from |
|---|---|---|---|
| `at_ms` | u64 | ms | monotonic driver time (`KeQueryInterruptTimePrecise / 10_000`) |
| `iin_ua` | u32 | µA | ADC channel `IIN`, LSB 4.89 mA |
| `vbat_uv` | u32 | µV | ADC channel `VBAT`, LSB 5 mV (Android: no +1 V offset; `ADC_VBAT_MIN` - the validity threshold) |
| `vin_uv` | u32 | µV | ADC channel `VIN`, LSB 16 mV |
| `die_temp_dc` | i32 | 0.1 °C | ADC channel `DIETEMP`, LSB 0.435 °C, offset -25 °C |
| `op_mode` | u8 | - | mode: 1 standby, 2 bypass 1:1, 3 switching 2:1 |
| `flags` | u8 | bit mask | bit 0 - fast mode, bit 1 - protection tripped, bit 2 - chip failure |

## 2. Charge session

Source: `crates/ln8000/src/session.rs`, structure `ChargeSession`.

| Field | Type | Unit | Meaning |
|---|---|---|---|
| `started_ms` | u64 | ms | the moment input power appeared |
| `duration_ms` | u64 | ms | duration; for the current session - "at the moment of the request" |
| `samples` | u32 | pcs | how many samples fell into the session |
| `peak_iin_ua` | u32 | µA | peak input current over the session |
| `peak_die_temp_dc` | i32 | 0.1 °C | peak die temperature |
| `had_fast_mode` | bool | - | whether 2:1 mode was engaged at least once |
| `had_guard_action` | bool | - | whether protection tripped (current reduction / bypass / stop) |
| `end_reason` | u8 | - | 0 open, 1 power lost, 2 stopped by protection, 3 chip failure |

## 3. Journal record on disk (JSON Lines)

Format: one JSON record per line, UTF-8 encoding, the file is appended to.
Source: `crates/host/src/journal.rs` + the `nabu-ln8000.ps1 journal` export.

| JSON field | Type | Meaning |
|---|---|---|
| `exported_at` | string | export time, ISO 8601 with zone |
| `host` | string | computer name (for the device matrix) |
| `soc_percent` | number | battery state of charge in % as reported by the OS (`Win32_Battery`), `-1` if the OS did not report it. The pump does not know it - the data comes from the system |
| `battery_status` | number | battery status by the WMI classification (`1` discharging, `2` on AC, etc.) |
| `pd_status` | number | negotiation status: `0` unknown, `1` ordinary 5 V adapter, `2` raised voltage 9 V and above, `3` QC negotiated. The value is set by whoever knows: in the acceptance protocol - from multimeter readings, in the report run - unknown |
| `pd_status_label` | string | the same in plain words |
| `mode` | number | pump mode: 1/2/3 |
| `state` | number | driver state: 1 identified ... 4 failure |
| `sys_sts` | number | `SYS_STS` as is (hex source: 0x03) |
| `fault1_sts`, `fault2_sts` | number | failure registers 0x05, 0x06 |
| `safety_sts` | number | protection register 0x04 |
| `critical` | 0/1 | flag of a critical failure |
| `iin_ua`, `vbat_uv`, `vbus_uv` | number | telemetry at the moment of the export |
| `die_temp_dc` | number | die temperature, 0.1 °C |
| `sessions` | number | how many sessions the driver completed |
| `samples` | number | how many samples were accumulated |

Records are read line by line; `jq` or `ConvertFrom-Json` over the lines is enough
for analysis. The point: **every session leaves a trace** suitable for analysis
after a reboot - this closes the "the journal survives a reboot" requirement
(only the current state is kept in driver memory).
