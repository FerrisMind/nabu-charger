# Pinned reference sources

Copies of the files that our code is checked against **by machine** with the
`deploy/verify-sources.ps1` script are placed here. The copies are needed so the check is
reproducible: a link to a file on the internet can change, but a pinned commit cannot.

## `ln8000_charger_extract.h`

| Field | Value |
|---|---|
| Source | <https://github.com/EmanuelCN/kernel_xiaomi_sm8250> |
| Path in the repository | `drivers/power/supply/lionsemi/ln8000_charger.h` |
| Commit (pinned) | `9e94307b9c1994cba25849099a0582ad3ab00390` |
| License | GPL-2.0 (Copyright (C) 2021 Lion Semiconductor Inc.) |
| Why | pins down the LN8000 register addresses and bit masks that were **missing** from the set in `04-android-reference-sources` (there the driver accesses registers by symbolic names, but the definitions themselves are not attached) |

This is an **excerpt**, not the whole file: the definitions needed for the cross-check
are kept (register addresses, bit masks, numeric constants, mode codes). To restore the
full file:

```powershell
Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/EmanuelCN/kernel_xiaomi_sm8250/9e94307b9c1994cba25849099a0582ad3ab00390/drivers/power/supply/lionsemi/ln8000_charger.h' -OutFile ln8000_charger.h
```

It is used only for cross-checking during development; nothing from it ends up in the
driver build, so the GPL here does not affect the license of our code (MIT/Apache-2.0).

## What exactly is cross-checked

* `enum ln8000_reg_addr` - register addresses (0x00 ... 0x4D);
* the bit masks `LN8000_MASK_*` and the bit numbers `LN8000_BIT_*`;
* the ADC channel, mode and OVP/watchdog timer threshold codes - cross-checked
  indirectly, through the encoding formulas and the core tests.

The result of the last cross-check - `artifacts/verify-sources.txt`.
