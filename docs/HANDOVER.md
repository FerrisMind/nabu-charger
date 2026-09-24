# Handover, build and rollback

A document for whoever takes over the project: how to build, how to run, how to
roll back and what to do about typical failures.

## 1. Architectures: what is built for what

| What | Target architecture | Why |
|---|---|---|
| `core`, `host`, `cli` (tests, CLI, benchmarks) | **x86_64-pc-windows-msvc** | These are developer tools; they run on the build machine (ours is a Ryzen, amd64) |
| `crates/kmdf` (kernel-mode driver) | **aarch64-pc-windows-msvc** | The target device is a Xiaomi Pad 5 (nabu, Snapdragon 860), Windows on ARM64 |

Both targets are declared in `rust-toolchain.toml`. The driver is built for ARM64
automatically: `crates/kmdf/.cargo/config.toml` pins
`[build] target = "aarch64-pc-windows-msvc"`. Without that file cargo would build
x86_64 (the host architecture), and the driver would not load on the tablet.

To check what something was built for:

```powershell
cargo build --workspace --release            # tools, x86_64
cd crates/kmdf; cargo wdk build              # driver, aarch64
```

## 2. Requirements

| Component | Version | Why |
|---|---|---|
| Rust | 1.97.0 (pinned in `rust-toolchain.toml`) | edition 2024 |
| rustup components | `rustfmt`, `clippy` | CI checks |
| rustup target | `aarch64-pc-windows-msvc` | the driver |
| Visual Studio | 2022 with C++ | linking |
| WDK | 10.0.26100.0 | KMDF headers and libraries |
| LLVM / clang | **17.0.6** | `bindgen` in `wdk-sys`; on 23.x the parsing of the WDF headers breaks |
| `cargo-wdk` | the latest, with `cargo install cargo-wdk --locked` | KMDF build |

The `kernel-driver` job in `.github/workflows/ci.yml` installs the last three of these
itself - the WDK through `winget`, LLVM 17.0.6 into `C:\llvm17` - so the versions in the
table are the ones to keep in step with that file. The commands below are the local
setup, and the LLVM path is the local one.

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

A driver build test-signs its package, and `cargo-wdk` takes the certificate from the user
store `WDRTestCertStore`; on a machine that has never built a driver the store is empty and
the build stops in the package step with `SignTool Error: File not found`. The store fills
with a `makecert` certificate on the first build that has no cached `.cer` next to its
output, which is what `deploy/prepare-test-signing.ps1` arranges for `ci.yml` and
`release.yml`.

## 3. Building and running the tools

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo test --workspace
cargo doc --workspace --no-deps
cargo build -p charger-core --no-default-features   # the core without std

cargo run -p cli -- --journal artifacts/journal-demo.jsonl demo
cargo run -p cli -- verify
```

The bench without hardware:

```powershell
# terminal 1
cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
# terminal 2
cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
```

## 4. Driver build

```powershell
cd crates/kmdf
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # LLVM 17.0.6
cargo wdk build --target-arch arm64 --profile release
```

Verified: the build ends with the message `Finished building kmdf`, the package
lands in `target/aarch64-pc-windows-msvc/release/kmdf_package/` and contains
`kmdf.sys`, `kmdf.inf`, `kmdf.cat` and a test signing certificate. The bitness
can be checked from the PE header:

```powershell
$sys = "crates/kmdf/target/aarch64-pc-windows-msvc/release/kmdf.sys"
$b = [IO.File]::ReadAllBytes($sys); $pe = [BitConverter]::ToInt32($b, 0x3C)
"Machine = 0x{0:X4}" -f [BitConverter]::ToUInt16($b, $pe + 4)   # 0xAA64 = ARM64
```

The ready package is also copied to `artifacts/driver-arm64/`.

Installation on the tablet (an administrator PowerShell **on the tablet**, test
signing already enabled):

```powershell
pnputil /add-driver .\kmdf.inf /install
# for a root device: create the node, then install the driver
# (devcon install kmdf.inf root\nabu_charger or through Device Manager)
```

## 5. Versioning and rollback

Versions follow [SemVer](https://semver.org/lang/ru/). Rules:

* `MAJOR` - an incompatible change of the IOCTL contract or the current policy;
* `MINOR` - new functionality without breaking the contract (for example, pump support);
* `PATCH` - fixes that do not change behaviour on healthy hardware.

**Before updating on the tablet:**

```powershell
# 1. save the current working artifact
Copy-Item .\nabu_charger.sys .\backup\nabu_charger-0.1.0.sys

# 2. the driver version is visible in the journal and in STAT VERSION
cargo run -p cli -- verify | Select-String "driver"
```

**Rollback to the previous working artifact:**

```powershell
pnputil /delete-driver oemNN.inf /uninstall     # where oemNN is the number from `pnputil /enum-drivers`
pnputil /add-driver .\backup\nabu_charger-0.1.0.inf /install
```

**Emergency rollback, if the tablet stopped charging altogether:** unload the driver
(`pnputil /delete-driver ... /uninstall`) and reboot. The driver does not change the
firmware and does not write to persistent storage - unloading restores the stock
Windows behaviour. That is why all registry and driver experiments are reversible.

## 6. Typical failures and what to do

| Symptom | Cause | What to do |
|---|---|---|
| `failed to select a version for the requirement wdk-sys` | the `wdk*` crates are versioned out of sync | use the combination from the official sample: `wdk 0.4.1` + `wdk-sys 0.5.1` + `wdk-build 0.5.1` |
| `wdk-sys (lib) ... attempt to compute 1_usize - 56_usize, which would overflow` | `bindgen` failed to parse the WDF headers: an incompatible libclang version | **solved:** install LLVM 17.0.6 and point `LIBCLANG_PATH` at its `bin`; then delete `crates/kmdf/target` and rebuild |
| `Error: StaticCrtNotEnabled` | the core links against the static CRT | `crates/kmdf/.cargo/config.toml` must have the flags `-C target-feature=+crt-static -C panic=abort` |
| `Missing .inx file in source path` | `cargo-wdk` requires an INF template | the file `crates/kmdf/kmdf.inx` is mandatory, the name matches the package name |
| `ERROR(1285): Cannot specify [ClassInstall32] section for Microsoft-defined class` | a class section is not allowed for Microsoft classes | do not declare `[ClassInstall32]` with `Class = System` |
| `Failed to rename ... kmdf.dll to kmdf.sys` | `cargo wdk build` without an architecture looks for the build in `target/debug` | always pass `--target-arch arm64` (then the artifacts are taken from `target/aarch64-pc-windows-msvc/...`) |
| `Failed to find function info for WdfGetTicks` | not all WDF functions are in the `cargo-wdk` table | for time in the kernel use `wdk_sys::ntddk::KeQueryInterruptTimePrecise` (100-ns ticks) |
| `not a valid rust project/workspace` from `cargo wdk` | the directory was not found or the manifest does not parse | run from `crates/kmdf`; make sure `Cargo.toml` has `[workspace]` |
| The driver built but does not load | test signing is not enabled or the driver was built for x86_64 | `bcdedit /set testsigning on` on the tablet and a reboot; check that the build was for `aarch64-pc-windows-msvc` |
| `read` returns `unsupported` | this is expected: the layout of the SPMI bus response is not confirmed by reverse engineering yet | see `docs/REGISTERS.md`, the "What remains to be clarified" section |
| Charging did not appear after the driver was installed | the hardware APSD detection did not complete | look at the journal (`artifacts/journal-*.jsonl`): the `detect` and `error` records show at which step it stopped |

## 7. The journal as an analysis tool

Every operation is written to JSON Lines:

```json
{"seq":42,"ts_ms":1500,"request_id":7,"level":"info","kind":"detect","adapter":"HVDCP3","raw_status":3,"raw_result":72,"waited_ms":1500}
```

What to look at during an incident:

* `kind=error` - the error code (`transport`, `detection_timeout`, `unknown_adapter_pattern`, ...);
* `kind=retry` - how many retries there were and for what reason;
* `kind=detect` - the raw register values: they show what the hardware actually returned;
* `kind=policy` - which current and voltage were set and why.

## 8. What comes next in the plan

1. Wire the detection timer to the queue: IOCTL `DETECT_START` -> timer ->
   `detect_step` -> `apply`. The logic is ready, the wiring is needed.
2. Finish the SPMI bus read path (confirm the response layout).
3. Install `kmdf.sys` on the tablet and check adapter detection and the current ramp.
4. The LN8000 charge pump driver (I2C 0x51) for the full 33 W.
5. Thermals and JEITA: current limiting by temperature.
