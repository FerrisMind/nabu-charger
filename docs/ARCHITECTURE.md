# Architecture

## Layers

```text
┌────────────────────────────────── user mode ───────────────────────────────────┐
│  nabu-charger (CLI)  ·  tests  ·  benchmarks  ·  diagnostics tools              │
└───────────────────────────────┬───────────────────────────────────────────────┘
                                │ IOCTL (METHOD_BUFFERED)
┌───────────────────────────────▼─────────────── OS kernel ─────────────────────┐
│  crates/kmdf - KMDF driver                                                     │
│    · EvtDeviceAdd: device, request queue, SPMI transport                        │
│    · EvtIoDeviceControl: GET_STATUS / DETECT_START / SET_ICL / READ_REG …       │
│    · timer: non-blocking detection steps                                       │
└───────────────────────────────┬───────────────────────────────────────────────┘
                                │ ChargerTransport (trait)
┌───────────────────────────────▼─────────────── core logic ────────────────────┐
│  crates/core - charger-core (no_std, no unsafe)                                │
│    · apsd      : parsing APSD -> adapter type                                  │
│    · policy    : type -> current limit, QC2 voltage, pump eligible             │
│    · icl       : current <-> register code (100 mA grid)                       │
│    · driver    : states, timeouts, retries, recovery, Drop                     │
│    · journal   : a record for every operation                                  │
└───────────────────────────────┬───────────────────────────────────────────────┘
                                │ transport implementation
        ┌───────────────────────┼───────────────────────┬───────────────────────┐
        ▼                       ▼                       ▼                       ▼
  SpmiTransport           MockTransport          TcpTransport          Simulator
  \Device\RESOURCE_HUB    registers in memory    network to bench/    TCP server with
  (real hardware)         (tests)                simulator            SMB registers
```

## Why the core is separated from the OS

Three reasons, each verified in practice:

1. **Testability.** All the logic, including timeouts and failure recovery, is
   verified without hardware: time comes from the `Clock` trait, so a timeout is
   reproduced instantly and deterministically.
2. **No `unsafe`.** There is not a single `unsafe` block in the core - all the
   unsafe code is concentrated in the transport (WDF, sockets). If a core revision
   finds a bug, it can be fixed in the driver without rebuilding the hardware.
3. **Portability of the checks.** The core also builds without `std`
   (`cargo build -p charger-core --no-default-features`) - that is exactly the form
   in which the kernel-mode driver uses it.

## Key decisions

| Decision | Rationale |
|---|---|
| The driver **does not block**: `detect_step()` returns `Pending` until the APSD is ready | In kernel mode one cannot sleep at an arbitrary place; the same code also works in the CLI, where the caller drives the loop |
| The core does not measure time itself (`Clock` from outside) | Deterministic timeout tests: `ManualClock::advance_ms` instead of waiting |
| All texts in the journal are static strings | Records are copyable, allocation-free, suitable for `no_std` |
| Errors are a custom enum with `core::error::Error` | Library code does not panic: no `unwrap`, `expect`, `panic`; the error reports whether work can continue (`is_recoverable`) |
| `Drop` restores a safe current limit | Unloading the driver must not leave the port in an unknown state; errors in `Drop` are swallowed and written to the journal |
| Register codes and addresses are constants from the Android driver | Behaviour on Windows must match the behaviour on Android, where charging works |

## Flow of one session

```text
open()            channel reset -> trial read of APSD_STATUS (link check)
detect_step()     APSD_STATUS -> the "detection complete" bit?
                  ├─ no: wait; on timeout - rerun APSD (CMD_APSD.APSD_RERUN)
                  └─ yes: APSD_RESULT_STATUS -> pattern -> table -> adapter type
apply(type)       policy: current limit -> USBIN_CURRENT_LIMIT_CFG (code on the 100 mA grid)
                  + enable in CMD_ICL_OVERRIDE
                  + for Quick Charge: voltage in HVDCP_PULSE_COUNT_MAX
                  + verification of every write by a read
monitor()         whether the adapter changed; on a change - apply() again
close()/Drop      safe current limit, a record in the journal
```

## Accepted assumptions

* The current limit encoding grid (minimum 100 mA, step 100 mA, 32 steps) is taken
  from the Android driver constants (`DCIN_ICL_MIN_UA`, `DCIN_ICL_STEP_UA`). The exact
  field width depends on the chip revision, so the grid is factored out into
  `IclEncoding` and verified by a read after the write.
* The adapter types and register patterns follow `smblib_apsd_results[]` from Android;
  QC3.5 is raised to `HVDCP3P5` only after authentication, otherwise `HVDCP3` is
  reported honestly.
* The layout of the SPMI bus response for a register read is not confirmed by
  reverse engineering - the corresponding path returns a typed failure, not a guess.
