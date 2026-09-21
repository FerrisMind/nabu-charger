# Provenance and licence

Copyright (C) 2026 FerrisMind. Licensed **GPL-2.0-or-later**; the full text is in
[LICENSE](LICENSE).

## The short version

This project is a **derivative work of GPL-2.0-or-later Linux kernel sources**. The LN8000
charge-pump driver was written for Android, there is no Windows driver for the chip, so
the register map, the bit masks, the numeric codes and the initialisation sequence were
taken from it and the encoding functions were rewritten in Rust against it. The project's
own module documentation says so in several places
(`crates/ln8000/src/regs.rs:1-6`, `docs/LN8000.md`, `CHANGELOG.md`).

Because of that the whole repository is GPL-2.0-or-later. An earlier version of
`reference/README.md` argued the opposite, on the grounds that the pinned reference file is
never compiled; that argument was wrong and has been removed. See "The argument that did
not hold" below.

## The sources, and what came from each

| Source | Licence | What was taken |
|---|---|---|
| `drivers/power/supply/ti/ln8000_charger.c` and `.h` (Lion Semiconductor / XiaoMi, 2021) | GPL-2.0-or-later | the LN8000 register map (42 addresses), every bit mask and bit number, the numeric constants and threshold codes, the ten-step initialisation sequence, and the bodies of six encoding functions (see below) |
| `drivers/power/supply/qcom/smb5-reg.h` (Qualcomm) | GPL-2.0 | the PM8150B USBIN register addresses and bit names used by `crates/core`, with the `_REG` / `_BIT` suffix dropped |
| `drivers/power/supply/qcom/smb5-lib.c` (Qualcomm) | GPL-2.0 | the APSD decode sequence and its table lookup, the QC3.5 authentication constants and pulse trains (`crates/core/src/qc35_auth.rs`, `crates/ln8000/src/hvdcp_policy.rs`), and one comment copied verbatim |
| `drivers/power/supply/qcom/battery.c` (Qualcomm) | GPL-2.0 | the current-stepping policy shape that `crates/core/src/policy.rs` follows |
| `drivers/power/supply/cp_qc30.c` | GPL-2.0 | one constant: `VBUS_COMP` = 250 mV, the headroom above `2 * Vbat` at which 2:1 is requested |
| the nabu device tree (`nabu-sm8150.dtsi`) | kernel | the property names and threshold values quoted in `driver.rs` and `docs/LN8000.md`; the DTS was not part of the provenance audit, so its own licence was not examined |

The pinned copy of the LN8000 header that the constants are checked against, and the
revision it is pinned to, are in [reference/](reference/).

## How close the port is

The audit read the C driver in full and compared it against the crate function by
function. The result is mixed, and the two halves should be kept apart:

**Close, sometimes mechanical.**

* All 42 register addresses, every bit mask and every numeric constant match value for
  value. The Rust names are re-coined (`DEVICE_ID` for `LN8000_REG_DEVICE_ID`), and no C
  identifier survives as a Rust identifier in `crates/ln8000`.
* `Pump::configure` follows `ln8000_init_device()` step for step, with three divergences:
  auto-recovery is fused with the two temperature monitors into one `RECOVERY_CTRL` write,
  the watchdog period is written where the C code has it inside `#if 0`, and the
  `tdie_prot` / `tdie_regulation` pair is swapped (same final register value).
* Six functions reproduce the body of a named C function with only mechanical rewrites:
  `AdcChannel::decode` against `ln8000_convert_adc_code`, `encode_vbat_float` against
  `ln8000_set_vbat_float`, `encode_vac_ovp` against `ln8000_set_vac_ovp`,
  `encode_ntc_alarm` against `ln8000_set_ntc_alarm`, `encode_iin_limit` against
  `ln8000_set_iin_limit`, and `sys_ctrl_bits` against `ln8000_change_opmode`.
  `AdcChannel::decode` is the most literal: the same eight channels, the same four
  bit-slicing masks, the same channel pairing, the same `935 - code` die-temperature
  formula and the same clamp bounds. `encode_iin_limit` is the one pair that changes
  behaviour: the Rust rejects a below-minimum value on the write path where the C clamps
  only on the read path.

**This project's own work.**

* `guard.rs`: the whole software fold-back ladder. The C driver delegates thermal handling
  to the chip and never reads the die temperature to reduce a current.
* `battery_policy.rs`: the online and charging holds, the evidence-gated update, the
  arming debounce. This is Windows power-presentation policy and has no counterpart.
* `session.rs`: telemetry ring buffer and charge sessions.
* The mode chooser. The C driver never requests 1:1 bypass at all; it is chosen by the
  platform. The Rust decides between standby, 1:1 and 2:1 from Vin and Vbat.
* The 2:1 window derivation from the pack voltage, and the predicates that reject the
  reflection and the hibernated ADC.
* The state machines in `hvdcp_policy.rs` and `qc35_auth.rs` (their constants are the C's,
  the decomposition is not).
* Everything under `crates/spb`, `crates/host`, `crates/cli`, `crates/kmdf` and
  `crates/ln8000-kmdf`: the Windows SPB, ACPI, KMDF and IOCTL layers have no Android
  counterpart by construction.

The findings that came out of running this on real hardware are in
[docs/FINDINGS.md](docs/FINDINGS.md); several of them describe behaviour the reference
driver never encounters, because on Android the chip is driven by the platform.

## The argument that did not hold

`reference/README.md` used to say that the pinned header "is used only for cross-checking
during development; nothing from it ends up in the driver build, so the GPL here does not
affect the license of our code (MIT/Apache-2.0)". It also said the definitions had been
missing from the Android reference sources. Both statements were wrong:

* The definitions are present in full in the reference sources on disk
  (`drivers_power_supply_ti_ln8000_charger.h`); the pinned excerpt is a superset of that
  copy by one register.
* The pinned file is indeed never compiled, but that is not the point: the same content is
  in `crates/ln8000/src/regs.rs`, which is compiled, and `crates/ln8000-kmdf` links it.
  The same holds for `crates/core` against `smb5-reg.h`.

## A copied comment, removed

One comment had been copied verbatim, misspelling included, from `smb5-lib.c:623` into
`crates/core/src/apsd.rs`. It has been rewritten in this project's own words. It was the
only verbatim quotation of C prose found anywhere in the repository; every other reference
to the C sources is an attribution naming the function, which is the opposite of a
concealment.

## How this was established, and what it does not establish

The comparison was made by reading the GPL sources in full and diffing them against the
crate function by function, not by sampling. `deploy/verify-sources.ps1` machine-checks the
numeric constants and the register and bit tables against the pinned header, so the numbers
are continuously verified; nothing in the repository verifies that the *decomposition* of a
function differs from the C's.

The audit is textual throughout. It establishes what the two trees contain, not what was
read, in what order, or with what intent. Where a question could not be settled from the
sources, it is recorded as open rather than resolved; the one that matters most is whether
`lionsemi/ln8282.c`, a second Lion mode-state-machine driver in the same reference tree, was
also consulted.
