# Передача, сборка и откат

Документ для того, кто получает проект: как собрать, как запустить, как
откатиться и что делать при типовых сбоях.

## 1. Архитектуры: что под что собирается

| Что | Целевая архитектура | Почему |
|---|---|---|
| `core`, `host`, `cli` (тесты, CLI, бенчмарки) | **x86_64-pc-windows-msvc** | Это инструменты разработчика; они запускаются на сборочной машине (у нас — Ryzen, amd64) |
| `crates/kmdf` (драйвер режима ядра) | **aarch64-pc-windows-msvc** | Целевое устройство — Xiaomi Pad 5 (nabu, Snapdragon 860), Windows on ARM64 |

Обе цели объявлены в `rust-toolchain.toml`. Драйвер собирается под ARM64
автоматически: в `crates/kmdf/.cargo/config.toml` зафиксирован
`[build] target = "aarch64-pc-windows-msvc"`. Без этого файла cargo собрал бы
x86_64 (архитектуру хоста), и драйвер не загрузился бы на планшете.

Проверить, подо что собрано:

```powershell
cargo build --workspace --release            # инструменты, x86_64
cd crates/kmdf; cargo wdk build              # драйвер, aarch64
```

## 2. Требования

| Компонент | Версия | Зачем |
|---|---|---|
| Rust | 1.97.0 (зафиксировано в `rust-toolchain.toml`) | edition 2024 |
| Компоненты rustup | `rustfmt`, `clippy` | проверки CI |
| Цель rustup | `aarch64-pc-windows-msvc` | драйвер |
| Visual Studio | 2022 с C++ | линковка |
| WDK | 10.0.26100.0 | KMDF-заголовки и библиотеки |
| LLVM / clang | **17.0.6** | `bindgen` в `wdk-sys`; на 23.x разбор заголовков WDF ломается |
| `cargo-wdk` | последний с `cargo install cargo-wdk --locked` | сборка KMDF |

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

## 3. Сборка и запуск инструментов

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo test --workspace
cargo doc --workspace --no-deps
cargo build -p charger-core --no-default-features   # ядро без std

cargo run -p cli -- --journal artifacts/journal-demo.jsonl demo
cargo run -p cli -- verify
```

Стенд без железа:

```powershell
# терминал 1
cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
# терминал 2
cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
```

## 4. Сборка драйвера

```powershell
cd crates/kmdf
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # LLVM 17.0.6
cargo wdk build --target-arch arm64 --profile release
```

Проверено: сборка заканчивается сообщением `Finished building kmdf`, пакет
ложится в `target/aarch64-pc-windows-msvc/release/kmdf_package/` и содержит
`kmdf.sys`, `kmdf.inf`, `kmdf.cat` и сертификат тестовой подписи. Разрядность
можно проверить по заголовку PE:

```powershell
$sys = "crates/kmdf/target/aarch64-pc-windows-msvc/release/kmdf.sys"
$b = [IO.File]::ReadAllBytes($sys); $pe = [BitConverter]::ToInt32($b, 0x3C)
"Machine = 0x{0:X4}" -f [BitConverter]::ToUInt16($b, $pe + 4)   # 0xAA64 = ARM64
```

Готовый пакет также скопирован в `artifacts/driver-arm64/`.

Установка на планшет (PowerShell от администратора **на планшете**, тестовая
подпись уже включена):

```powershell
pnputil /add-driver .\kmdf.inf /install
# для root-устройства: создать узел, затем установить драйвер
# (devcon install kmdf.inf root\nabu_charger или через диспетчер устройств)
```

## 5. Версионирование и откат

Версии — по [SemVer](https://semver.org/lang/ru/). Правила:

* `MAJOR` — несовместимое изменение контракта IOCTL или политики тока;
* `MINOR` — новая функциональность без слома контракта (например, поддержка pump);
* `PATCH` — исправления без изменения поведения на исправном железе.

**Перед обновлением на планшете:**

```powershell
# 1. сохранить текущий рабочий артефакт
Copy-Item .\nabu_charger.sys .\backup\nabu_charger-0.1.0.sys

# 2. версия драйвера видна в журнале и в STAT VERSION
cargo run -p cli -- verify | Select-String "версия"
```

**Откат к предыдущему рабочему артефакту:**

```powershell
pnputil /delete-driver oemNN.inf /uninstall     # где oemNN — номер из `pnputil /enum-drivers`
pnputil /add-driver .\backup\nabu_charger-0.1.0.inf /install
```

**Аварийный откат, если планшет перестал заряжаться вовсе:** выгрузить драйвер
(`pnputil /delete-driver … /uninstall`) и перезагрузиться. Драйвер не меняет
прошивку и не пишет в постоянную память — выгрузка возвращает штатное поведение
Windows. Именно поэтому все эксперименты с реестром и драйвером обратимы.

## 6. Типовые сбои и действия

| Симптом | Причина | Что делать |
|---|---|---|
| `failed to select a version for the requirement wdk-sys` | крейты `wdk*` версионируются несинхронно | использовать связку из официального семпла: `wdk 0.4.1` + `wdk-sys 0.5.1` + `wdk-build 0.5.1` |
| `wdk-sys (lib) ... attempt to compute 1_usize - 56_usize, which would overflow` | `bindgen` не разобрал заголовки WDF: несовместимая версия libclang | **решено:** установить LLVM 17.0.6 и указать `LIBCLANG_PATH` на его `bin`; затем удалить `crates/kmdf/target` и пересобрать |
| `Error: StaticCrtNotEnabled` | ядро линкуется со статическим CRT | в `crates/kmdf/.cargo/config.toml` должны быть флаги `-C target-feature=+crt-static -C panic=abort` |
| `Missing .inx file in source path` | `cargo-wdk` требует шаблон INF | файл `crates/kmdf/kmdf.inx` обязателен, имя совпадает с именем пакета |
| `ERROR(1285): Cannot specify [ClassInstall32] section for Microsoft-defined class` | для классов Microsoft секция класса запрещена | не объявлять `[ClassInstall32]` при `Class = System` |
| `Failed to rename ... kmdf.dll to kmdf.sys` | `cargo wdk build` без указания архитектуры ищет сборку в `target/debug` | всегда передавать `--target-arch arm64` (тогда артефакты берутся из `target/aarch64-pc-windows-msvc/...`) |
| `Failed to find function info for WdfGetTicks` | не все WDF-функции есть в таблице `cargo-wdk` | для времени в ядре использовать `wdk_sys::ntddk::KeQueryInterruptTimePrecise` (100-нс тики) |
| `not a valid rust project/workspace` от `cargo wdk` | каталог не найден или манифест не парсится | запускать из `crates/kmdf`; убедиться, что в `Cargo.toml` есть `[workspace]` |
| Драйвер собрался, но не грузится | не включён test signing или драйвер собран под x86_64 | `bcdedit /set testsigning on` на планшете и перезагрузка; проверить, что сборка шла под `aarch64-pc-windows-msvc` |
| `read` возвращает `unsupported` | это ожидаемо: раскладка ответа шины SPMI ещё не подтверждена реверсом | см. `docs/REGISTERS.md`, раздел «Что осталось выяснить» |
| Заряд не появился после установки драйвера | аппаратная детекция APSD не завершилась | смотреть журнал (`artifacts/journal-*.jsonl`): записи `detect` и `error` показывают, на каком шаге остановились |

## 7. Журнал как инструмент разбора

Каждая операция пишется в JSON Lines:

```json
{"seq":42,"ts_ms":1500,"request_id":7,"level":"info","kind":"detect","adapter":"HVDCP3","raw_status":3,"raw_result":72,"waited_ms":1500}
```

Что смотреть при инциденте:

* `kind=error` — код ошибки (`transport`, `detection_timeout`, `unknown_adapter_pattern`, …);
* `kind=retry` — сколько было повторов и по какой причине;
* `kind=detect` — сырые значения регистров: по ним видно, что именно вернула аппаратура;
* `kind=policy` — какой ток и напряжение выставлены и почему.

## 8. Что дальше по плану

1. Подключить таймер детекции к очереди: IOCTL `DETECT_START` → таймер →
   `detect_step` → `apply`. Логика готова, нужна проводка.
2. Довести read-путь шины SPMI (подтвердить раскладку ответа).
3. Установить `kmdf.sys` на планшет и проверить детекцию блока и рост тока.
4. Драйвер charge pump LN8000 (I2C 0x51) для полных 33 Вт.
5. Термика и JEITA: ограничение тока по температуре.
