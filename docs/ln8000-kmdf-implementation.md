# LN8000: реализация KMDF (Rust)

Прикладные шаги для `crates/kmdf` и будущего SPB-транспорта к LN8000.

Общий контекст: `01-analysis/ln8000-windows-driver-roadmap.md`.

---

## Архитектура

```
[Battery miniclass / CLI]  ←→  [KMDF LN8000]  ←→  SpbCx / I²C (QUP)
                                      ↑
                               ln8000_charger.c (логика)
                               core crate (политика, если нужна)
```

Текущий `kmdf` крейт — каркас WDK; SPMI/SPB-транспорт — в разработке (`crates/kmdf/src/spmi.rs`).

---

## Шаг 1 — SPB / I²C

1. INF: `HardwareIds` = `ACPI\QCOM057E` (существующий PEIC) или свой `LNX8000`.
2. `EVT_WDF_DEVICE_PREPARE_HARDWARE`: разбор `_CRS`, `SpbTargetDeviceConnect`.
3. Probe: `SpbRead` регистра `0x00` → `DEVICE_ID == 0x42`.

Образцы: `08-driver-samples/Windows-driver-samples/spb/`.

---

## Шаг 2 — Init (из DTS)

Пороги и disable-флаги — таблица в `04-android-reference-sources/LN8000.md`.

Минимальный набор записей после probe:

- `THRESHOLD_CTRL`, `NTC_CTRL`
- `FAULT_CTRL` (учесть отключённые в DT защиты)
- `REGULATION_CTRL`, `IIN_CTRL`, `V_FLOAT_CTRL`

---

## Шаг 3 — IRQ

- Зарегистрировать `WdfInterruptCreate` по GPIO из `_CRS` (GPIO **36** на nabu).
- В DPC: читать `INT1`, `SYS_STS`, `FAULT1/2_STS`, `SAFETY_STS`.
- Маски: `INT1_MSK`.

---

## Шаг 4 — op_mode

Целевое состояние: **`LN8000_OPMODE_SWITCHING` (3)**.

Управление через `SYS_CTRL` (`STANDBY_EN`, `EN_1TO1`), `CHARGE_CTRL`, `REGULATION_CTRL`.

**Acceptance test:** после включения ЗУ `op_mode == 3` стабильно; откат к `1` (STANDBY) = баг init/PD/защит.

---

## Шаг 5 — IOCTL / интеграция

Варианты:

- отдельный control device + IOCTL (как в `crates/kmdf/src/ioctl.rs`);
- связка с battery miniclass через shared interface / WMI.

Не дублировать `qcbattmngr8150` без координации.

---

## Шаг 6 — сборка и отладка

```text
# из 11-driver-rust/crates/kmdf (отдельный workspace, ARM64)
cargo wdk build --target aarch64-pc-windows-msvc
```

- Test signing, Secure Boot off.
- WinDbg: `!wdfkd.wdfdevice`, SPB trace.
- Последовательность: **DEVICE_ID → пороги → op_mode 3 → IRQ**.

---

## Зависимости вне LN8000

| Блок | Статус под Windows |
|---|---|
| Fuel gauge PM8150 | ✅ |
| SMB5 / APSD | ❌ (отдельный трек, `core` crate) |
| PM8150B PD / Type-C | ❌ (нужен для 9 V+) |

---

## Ссылки

- Регистры: `04-android-reference-sources/drivers_power_supply_ti_ln8000_charger.h`
- Логика: `04-android-reference-sources/drivers_power_supply_ti_ln8000_charger.c`
- ACPI: `09-acpi-nabu/ln8000-acpi-uefi.md`
