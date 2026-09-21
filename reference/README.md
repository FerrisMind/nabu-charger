# Pinned reference sources

Copies of the GPL sources this project was ported from. They are pinned so that the
provenance statement in [PROVENANCE.md](../PROVENANCE.md) can be checked against the
exact revision that was used: a link to a file on the internet can change, but a
pinned commit cannot.

## `ln8000_charger_extract.h`

| Field | Value |
|---|---|
| Source | <https://github.com/EmanuelCN/kernel_xiaomi_sm8250> |
| Path in the repository | `drivers/power/supply/lionsemi/ln8000_charger.h` |
| Commit (pinned) | `9e94307b9c1994cba25849099a0582ad3ab00390` |
| License | GPL-2.0-or-later (Copyright (C) 2021 Lion Semiconductor Inc.) |
| Why | pins the revision of the header that the register map, the bit masks and the numeric codes in `crates/ln8000` were ported from |

This is an **excerpt**, not the whole file: the definitions the port uses are kept
(register addresses, bit masks, numeric constants, mode codes). To restore the full
file:

```powershell
Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/EmanuelCN/kernel_xiaomi_sm8250/9e94307b9c1994cba25849099a0582ad3ab00390/drivers/power/supply/lionsemi/ln8000_charger.h' -OutFile ln8000_charger.h
```

## What is cross-checked

* `enum ln8000_reg_addr` - register addresses (0x00 ... 0x4D);
* the bit masks `LN8000_MASK_*` and the bit numbers `LN8000_BIT_*`;
* the ADC channel, mode and OVP/watchdog timer threshold codes - cross-checked
  indirectly, through the encoding formulas and the core tests.

## This content is in the build, not beside it

Two statements in an earlier version of this file were wrong and have been removed: that
the definitions had been **missing** from `04-android-reference-sources`, and that
nothing from the excerpt ends up in the driver build.

The definitions are present in the reference sources on disk as well
(`drivers_power_supply_ti_ln8000_charger.h`, the XiaoMi variant of the same driver); the
pinned extract fixes one specific upstream revision to compare against. And the same
register addresses, bit masks and numeric codes are in the build:
`crates/ln8000/src/regs.rs` carries them, and `crates/ln8000-kmdf` links that crate.
`crates/core` is in the same position with respect to `smb5-reg.h` and `smb5-lib.c`.

That is why the repository is licensed **GPL-2.0-or-later** rather than MIT/Apache, and
why the provenance is written out in [PROVENANCE.md](../PROVENANCE.md) instead of being
argued from the fact that the reference files themselves are not compiled.
