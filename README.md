# nabu-fastcharge

Драйвер зарядки для **Xiaomi Pad 5 (nabu, Snapdragon 860) под Windows on ARM64**.

Задача: под Windows планшет не заряжается ни от одного блока питания — ни от
USB-A (Quick Charge), ни от USB-C (Power Delivery). Причина не в «отсутствии
быстрой зарядки», а в том, что **аппаратная детекция адаптера (APSD) в PMIC
выполняется, но ни один компонент Windows её результат не читает**, поэтому
лимит входного тока не поднимается и заряд не идёт.

Драйвер закрывает ровно этот пробел: читает результат детекции и применяет
политику тока.

## Состав

| Крейт | Что это | Проверка |
|---|---|---|
| [`crates/core`](crates/core) | ядро логики: детекция APSD, политика тока, состояния, таймауты, журнал. Без `std`, без `unsafe` | 37 unit-тестов + doctests |
| [`crates/host`](crates/host) | хост-слой: транспорты (мок, TCP), журнал JSONL, `tracing`, симулятор устройства, бенчмарки | 9 интеграционных тестов |
| [`crates/cli`](crates/cli) | утилита `nabu-charger`: `demo`, `detect`, `sim`, `verify` | 4 теста CLI |
| [`crates/kmdf`](crates/kmdf) | драйвер режима ядра (KMDF) для ARM64 через шину SPMI | собран: `kmdf.sys` ARM64, подписан, `infverif` пройден |

## Быстрый старт

```powershell
# 1. Проверки (то же, что гоняет CI)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 2. Демонстрация: все типы адаптеров на мок-транспорте + журнал
cargo run -p cli -- --journal artifacts/journal-demo.jsonl demo

# 3. Самопроверка таблиц и математики драйвера
cargo run -p cli -- verify

# 4. Стенд без железа: симулятор устройства + реальный TCP-транспорт
cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
```

Ожидаемый вывод `demo` (проверено, см. `artifacts/verify-demo.txt`):

```text
сценарий   итог   адаптер    ток,мкА pump   пояснение
------------------------------------------------------------------------------
HVDCP3P5   ок     HVDCP3     3000000 да     Quick Charge 3.0, 9 В и 3 А, возможен charge pump
HVDCP3     ок     HVDCP3     3000000 да     Quick Charge 3.0, 9 В и 3 А, возможен charge pump
HVDCP2     ок     HVDCP2     1500000 нет    Quick Charge 2.0, 9 В и 1.5 А
DCP        ок     DCP        1500000 нет    порт только зарядки, BC1.2 1.5 А
SDP        ок     SDP        500000  нет    стандартный порт USB, предел 500 мА
DETACHED   отказ  —          —       —      ошибка detection_timeout — питание отсутствует: ожидается таймаут
UNKNOWN    отказ  —          —       —      ошибка unknown_adapter_pattern — неизвестный образец: ожидается отказ
```

## Как это работает

```text
клиент (IOCTL) ──► драйвер KMDF ──► ядро логики ──► транспорт ──► \Device\RESOURCE_HUB (SPMI) ──► SMB в PM8150B
                                     │
                                     ├─ читает APSD_STATUS / APSD_RESULT_STATUS
                                     ├─ разбирает тип адаптера (таблица из Android)
                                     └─ пишет лимит входного тока и напряжение QC2
```

Ядро не знает ни про Windows, ни про ввод-вывод: оно работает поверх трейта
[`ChargerTransport`](crates/core/src/transport.rs), а время и журнал получает
извне. Поэтому вся логика, включая таймауты и восстановление после сбоев,
проверяется без железа.

Архитектура подробно: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
Карта регистров: [docs/REGISTERS.md](docs/REGISTERS.md).

## Ограничения и честные оговорки

* **Драйвер режима ядра** (`crates/kmdf`) собирается под ARM64
  (`cargo wdk build --target-arch arm64`): `kmdf.sys` подписан, INF прошёл
  `infverif`, разрядность подтверждена по PE (`0xAA64`). Пакет лежит в
  `artifacts/driver-arm64/`. Требуется LLVM **17.x** для `bindgen`.
* **Таймер детекции** в драйвере ещё не подключён к очереди: IOCTL
  `DETECT_START`/`APPLY_POLICY`/`GET_JOURNAL` возвращают `STATUS_NOT_IMPLEMENTED`.
  Логика под ними готова и проверена на моках — осталась проводка в ядре.
* **Read-путь** транспорта SPMI пока возвращает типизированный отказ: раскладка
  ответа шины не подтверждена реверсом до конца (см. `TODO(RE)` в
  `crates/kmdf/src/spmi.rs`). Догадка не выдаётся за факт.
* **Charge pump (LN8000)** этим драйвером не управляется: для полных 33 Вт нужен
  отдельный драйвер I2C (адрес 0x51), это следующий этап.

## Сборка драйвера

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # нужен LLVM 17.x
cd crates/kmdf
cargo wdk build --target-arch arm64 --profile release
# результат: target/aarch64-pc-windows-msvc/release/kmdf_package/
```

## Лицензия

Двойная: MIT или Apache-2.0, на выбор. См. [LICENSE-MIT](LICENSE-MIT) и
[LICENSE-APACHE](LICENSE-APACHE).
