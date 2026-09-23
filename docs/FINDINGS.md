# Hardware and platform findings: charging on the Xiaomi Pad 5 (nabu) under Windows

This is the reusable part of the project: what was learned about the hardware and about Windows
while building a charge-pump driver for the Xiaomi Pad 5 (Snapdragon 860, PM8150B main charger,
LN8000 2:1 pump at I²C address 0x51 on the ACPI node `PEIC` / `QCOM057E`). It is written for two
readers: someone porting Windows to a nabu-class tablet, and someone working on Qualcomm charge-pump
charging (SMB5 APSD / Quick Charge, or an LN8000-style 2:1 pump).

Each finding ends with an `Evidence:` line. Paths are relative to the repository root and refer to
this tree; `docs/STATE-2026-09-17.md` and `docs/COMPAT-MATRIX.md` are the records of what was
measured on the tablet rather than verified in code. Where a claim rests on reasoning rather than a
measurement or a line of code, it is marked `[inference]`. Where a part of a finding is understood
and a part is not, both are stated.

Platform under test: Windows 11 25H2 ARM64 (build 26200.9445), Qualcomm driver package 2608.03,
test signing on. The driver reads the pump over I²C through the ACPI resource node
(`\Device\RESOURCE_HUB\<id>`), and the PMIC registers over `\Device\Spmi\SUPERUSER`.

---

## 1. Windows never reads the PMIC adapter-detection result

The PM8150B performs hardware adapter detection (APSD) by itself and publishes the result in two
registers of the USBIN peripheral: `APSD_STATUS` (0x1307, bit 0 = detection done, bit 1 = QC-capable
adapter) and `APSD_RESULT_STATUS` (0x1308, the adapter pattern in bits 6:0). No stock Windows
component consumes either register. A scan of the strings and IOCTLs of the whole Qualcomm 2608.03
package finds no `APSD`, no `QC2`/`QC3`, no `DCP`, no `charger_type`; the ACPI `_DSM` of the USB
node exposes only the standard USB controller capabilities and no current-control interface. The
consequence is mechanical: the input current limit register (`USBIN_CURRENT_LIMIT_CFG`, 0x1370) is
never raised as a function of the detected adapter type, so a supply that needs that step up is
never asked for its current.

What is measured, as opposed to inferred: with a USB-A supply the tablet does not charge at all
under stock Windows; with a PD supply the OS reports AC but capacity growth was not demonstrated
(see the open items below); the first charge-rate measurements based on `GetSystemPowerStatus` were
later found to rest on an unreliable signal (that call returned 255 = unknown), and only the pump's
own ADC and the battery voltage were trustworthy.

The QcUsbFnSs `HVDCP` flag was set to 1 and the Type-C nodes were restarted; the input stayed at
4.816 V, so the missing consumer is not a disabled registry switch. Closing the gap requires an
actual reader of the APSD result and a writer of the current limit; this repository's `core` crate
is one implementation of that missing component (`crates/core`), with the decode tables, the policy
(current per adapter type) and the 100 mA-grid encoder.

Evidence: `crates/core/src/lib.rs:10-13` (statement of the gap); `crates/core/src/apsd.rs:100-156`
(decode of `APSD_STATUS` + `APSD_RESULT_STATUS`); `crates/core/src/policy.rs:101-153` (current per
adapter type: SDP 500 mA, CDP/DCP 1.5 A, HVDCP2 1.5 A, HVDCP3 3 A, HVDCP3.5 3 A);
`crates/core/src/icl.rs:14-20` (100 mA grid, 32 steps); `crates/core/src/regs.rs:13-31,41-51`
(register map, `ICL_OVERRIDE | ICL_OVERRIDE_AFTER_APSD`); `docs/COMPAT-MATRIX.md:66-69` and
`docs/COMPAT-MATRIX.md:113-117` (the scan and the failed registry lever);
`docs/STATE-2026-09-17.md:180-182` (the measured consequence).

Correction against the summary this was written from: the crate's own doc comment says the tablet
"does not charge from any power brick". The measured record is narrower - a USB-A supply does not
charge, a PD supply charges slowly through the stock path, and whether the pack gains capacity from
a PD brick was not demonstrated with a reliable instrument.

---

## 2. The LN8000 ADC auto-hibernates, and after that it reads successfully but returns zeros

Initialisation step 9 writes `ADC_CTRL` (0x23) in three passes: first bits 5:7 = `Shutdown`, then
bits 3:4 = `Sec4` (the hibernation entry delay), then bits 5:7 = `AutoHibernate`. The register value
that results is 0x1C when the NTC-alarm high bits are zero, and the live dump on 19.09 with the
cable unplugged read `ADC_CTRL = 0x1C`. From then on, roughly four seconds of pump idle are enough
for the ADC to go to sleep; after that every channel register the driver reads
(`ADC01`..`ADC09`, addresses 0x09..0x12: IIN, VAC, VIN, VOUT, VBAT, DIETEMP, TSBAT, TSBUS) answers
*without a bus error* and contains 0x00.

This is the part of the finding that is easy to get wrong: `is_ok()` on the transfer means "the
register answered", not "the channel is alive". Zero is a valid register value, so nothing in the
transport layer reports a problem. In the decoder a zero raw code is also not a plausible
measurement: VBAT raw 0 decodes to 0 V, and zero volts on a live Li-ion cell is impossible, which is
why `vbat_reading_usable` requires both a successful read and a non-zero value. Die temperature raw
0 decodes to +160.0 °C, which the guard then rejects as implausible
(`DIE_TEMP_MAX_PLAUSIBLE_DC` = +125.0 °C). The operational consequence is the point of the finding:
a zero is "no sample", never "zero volts", and any code that feeds such a zero into a power
decision will conclude that the adapter was unplugged (a zero `vbus` is below every Vin threshold,
so the online predicate answers false).

In this driver the validity travels beside the value: the KMDF tick collapses a failed read to zero
with `unwrap_or_default()` and then computes `vbat_valid` from the read result *and* the value, and
`input_readings_usable` from the same facts. The reference Android driver does not rely on this;
it enables the channels with `ADC_CFG = 0x3E` and reads them while the pump is working, which is
exactly the regime where the hibernation does not occur.

Evidence: `crates/ln8000/src/driver.rs:477-509` (init step 9: watchdog, `adc_off_before_config`,
`adc_hibernate_delay`, `ADC_CFG = 0x3E`, `adc_auto`); `crates/ln8000/src/encoding.rs:403-452`
(the delay code `Sec4 = 3` and the mode code `AutoHibernate = 0`);
`crates/ln8000/src/encoding.rs:279-296` (`vbat_reading_usable`; the 19.09 measurement is in the
comment). `crates/ln8000/src/encoding.rs:848-862` (test: 0x1C decodes to AutoHibernate + Sec4, and a
successful zero is still not a usable VBAT); `crates/ln8000-kmdf/src/lib.rs:2511-2514`
(`unwrap_or_default`), `:2542-2543` (`vbat_valid`, `die_temp_valid`), `:2554`
(`input_readings_usable`), `:2576-2593` (`AdcValid` and `InputUsable` marks);
`crates/ln8000/src/guard.rs:24-31,349-351` (zero raw code decodes to +160.0 °C; the guard rejects
it).

Not settled: the four-second figure comes from the vendor delay enum (`Sec4`) and from the
reference Android driver, not from a datasheet read here. What was measured is that a sleeping ADC
returns zeros in every channel while a live cell is connected; the entry delay itself was not timed.

---

## 3. The phantom 2 * VBAT on an input that is no longer there

With the brick unplugged, the pump's VIN node is unloaded and the ADC can still report a voltage:
it reads approximately twice the pack voltage. The recorded case (19.09) is `Vin = 8.800 V` with
`VBAT = 4.400 V` - exactly half - with the input current at the 39 mA ADC floor and the pump in
standby. A naive "is VBUS above VBAT" test therefore reports a live input that is not there; on
this tablet the tray showed "connected" while the pack was discharging.

The guard this project uses is `vin_is_doubled_vbat(vbat_uv, vin_uv)`: it is true when VBAT sits
within ±80 mV (`VBAT_VIN_HALF_SLACK_UV`) of `Vin / 2`, and false for a zero pack reading. In
`online_raw` the checks are ordered deliberately: current first (Vin >= 4.2 V and Iin >= 80 mA
proves an adapter on its own, whatever the register says), then the hardware `VAC_UNPLUG` bit, then
this reflection veto, then the elevated-bus branch (Vin >= 6.0 V), then the 4.6 V floor, then a
required `Vin >= VBAT + 200 mV`. The order matters because the reflection scales with the pack: on
a discharged cell it lands between 4.2 and 8.0 V, so a veto placed after the "elevated" branch would
never see it. A second, narrower predicate (`vbat_tracks_converter_rail`) applies the same test only
above 8.0 V; it is used to keep the reflection out of the taper and converter-ceiling decisions.

Evidence: `crates/ln8000/src/encoding.rs:238-280` (both predicates and the measured case in the
comment); `crates/ln8000/src/battery_policy.rs:137-203` (`online_raw` and the documented order),
`:426-437` and `:556-593` (tests using the measured 8.800 V / 4.400 V / 39 mA point);
`crates/ln8000-kmdf/src/lib.rs:2525-2533` (`phantom_input` for the protection and session paths).

Not settled: the code states that the unloaded VIN node reads 2 * VBAT and treats it as a reflection
of the converter's own rail; no measurement here distinguishes a real residual voltage on the input
capacitors from an ADC artifact. What is established is the value and the fact that acting on it as
a live input produced a wrong tray state.

---

## 4. FAULT1 semantics: bit 4 is the hardware's own "the input is gone" verdict

The bit map this project uses for the LN8000 `FAULT1_STS` register (0x05):

| Bit | Mask | Name used here | Meaning |
|---|---|---|---|
| 7 | 0x80 | `FAULT1_WATCHDOG` | watchdog timer expired |
| 6 | 0x40 | `FAULT1_VBAT_OV` | battery overvoltage |
| 4 | 0x10 | `FAULT1_VAC_UNPLUG` | input (VAC) disconnected |
| 3 | 0x08 | `FAULT1_VAC_OV` | input overvoltage |
| 1 | 0x02 | `FAULT1_VIN_OV` | VIN overvoltage |
| 6:0 | 0x7F | `FAULT1_VFAULTS_MASK` | the vendor's "voltage faults" group |

Bits 5, 2 and 0 are unnamed in the public vendor driver and this project deliberately gives them no
names: a live `FAULT1 = 0x21` is two unnamed bits of that group. That is why
`has_critical_fault()`, which looks only at named bits, stays silent on such a frame while the
vendor's own `volt_qual` (the whole group must be clean) is false. Both signals are published so
that "no critical fault" cannot be misread as "the input is valid".

Bit 4 is the interesting one. The vendor answers the question "is VBUS present" *with this bit*
(`POWER_SUPPLY_PROP_TI_VBUS_PRESENT` maps to `!vac_unplug`), so it is the hardware's own verdict
rather than a threshold applied to a sample that may be a hibernated zero. It therefore has to be
able to end an "adapter online" verdict even when no ADC reading exists at all. That is exactly what
`online_raw_with_evidence` does: with usable readings it runs the ordinary chain; with unusable
readings and bit 4 set it returns `Some(false)` (the hold may age); with unusable readings and bit 4
clear it returns `None` (no verdict, the hold must not age). Bit 4 is deliberately not allowed to
override real current: if Iin is above the charging floor, an adapter is there and a stale latch
cannot drop the input.

The live `0x30` frame decodes to bit 4 (`VAC_UNPLUG`) plus unnamed bit 5. It was measured on 19.09
with the cable unplugged: `FAULT1 = 0x30`, `Vin = 8.80 V` with the cell at 4.40 V, current 39 mA -
i.e. the hardware said "unplugged" while the ADC still showed an 8.8 V bus (finding 3).

Evidence: `crates/ln8000/src/regs.rs:26,147-175` (address and all named bits, plus the note that
bits 5, 2 and 0 are unnamed); `crates/ln8000/src/status.rs:215-244` (`volt_qual` vs
`has_critical_fault`); `crates/ln8000/src/battery_policy.rs:166-203` (bit 4 in `online_raw`, with
the 0x30 measurement in the comment) and `:240-255` (`online_raw_with_evidence`);
`crates/ln8000-kmdf/src/lib.rs:2519-2524` (bit 4 read fresh in the same tick);
`docs/LN8000.md:26-58` (register map; note that `docs/REGISTERS.md` covers the PM8150B USBIN
peripheral, not the LN8000 `FAULT1`).

---

## 5. The hold asymmetry that produces a Windows-visible phantom power-source change

Two constants set the hysteresis on the battery-class flags (`crates/ln8000/src/battery_policy.rs`):

* `ONLINE_HOLD_MS = 8_000` - once the adapter has been seen online, `POWER_ON_LINE` stays published
  until the raw predicate has been false *continuously* for 8 s;
* `CHARGING_HOLD_MS = 20_000` - the same for the charging flag, longer because the ADC floor
  (39 mA) lasts through QC3 pulses and mode transitions;
* `HOLD_ARM_RUN = 2` - a hold arms only after two consecutive raw-true samples, so a lone true
  sample between false ones cannot re-arm the window (`IIN_CHARGING_UA = 80_000` is the raw
  charging floor).

`POWER_ON_LINE` follows the online hold and `CHARGING` follows the charging hold, gated by the
online hold; `DISCHARGING` is derived from the held online value, so a one-tick dropout never shows
as discharging. The flags are published to BattC, and `BatteryClassStatusNotify` is called on a
power-state or percentage change.

Combined with finding 2, one failure mode is arithmetic: a pump-idle stretch longer than the ADC
hibernation (about 4 s) plus the 8 s online window publishes AC -> DC -> AC to Windows while the
cable never moved. On this platform that is not cosmetic: a power-source change makes Windows
re-apply the power policy, including the display brightness policy, which is how the bug was
noticed by the operator. The measured size of the symptom is in the code comment that motivated the
fix: Kernel-Power 105 (power source change) fired 11 times in 12 seconds.

**That is not the only route, and in a live charging session on 21.09 it was not the route that
fired.** The session produced a genuine phantom AC -> DC -> AC with the cable motionless in which
the ADC was *awake* throughout: `InputUsable = 1`, `AdcValid` bit 0 clear and a plausible non-zero
VBAT in every row, so the `None` branch above was never reachable and the driver's own branch mark
read `usable` across the whole event. The decision that fired was `vin_is_doubled_vbat`. With the
pump out of switching, `Iin` sits on the 39 mA ADC floor, below `IIN_CHARGING_UA`, so the current
guard does not answer; `Vin` then relaxes to `2 · VBAT`, which for a 2:1 charge pump is the *normal*
operating point rather than evidence of absence. The slack is 80 mV, and on the deciding tick
`Vin = 8 272 000` against `VBAT = 4 140 000` — 4 mV from exact doubling — so the veto answered "no
adapter" while `Fault1Sts` bit 4 was *clear*, meaning the hardware's own VBUS-present detector said
the cable was there. The 8 s hold then expired 8.006 s after the last non-doubled sample, and
Windows recorded `Kernel-Power 105 AcOnline=false` at 22:41:45.536 followed by `AcOnline=true` at
22:41:48.183: 2.647 s of the OS believing the tablet was on battery, cable untouched.

The two are therefore distinct mechanisms behind the same symptom, and a fix that diverts only the
unusable tick — `online_raw_with_evidence` — closes the hibernation route and leaves the
doubled-VBUS route open. Both live in `online_raw` and both are decided by the order of its guards,
which is why the tests in this module pin the order as much as the outcomes.

### The doubled-VBUS route, captured with an OS-side sampler (22.09)

A 20-minute trace sampled the battery-class view (`root\wmi BatteryStatus`, i.e. what the tray and
Windows read) every 250 ms and the driver's own telemetry every eighth sample. The cable was never
touched, and `FAULT1` bit 4 read `0x00` in all 346 telemetry samples — the hardware's VBUS-present
verdict never wavered. Windows was still told "on battery" three times:

| published dropped | returned | DC window | electrical state during the window |
| --- | --- | --- | --- |
| 759 926 ms | 762 186 ms | 2.26 s | mode 1 `STANDBY`, `Iin` 39 120 µA (the floor), `Vin` 8 624 000 µV |
| 787 637 ms | 788 595 ms | 0.96 s | mode 1, `Iin` 39 120, `Vin` 8 608 000-8 832 000 |
| 954 265 ms | 957 037 ms | 2.77 s | mode 1, `Iin` 39 120, `Vin` 8 624 000 |

Each published window is the *tail* of a longer false run: the hold absorbs the first eight seconds,
so the OS sees only the last one to three. The electrical signature is identical in all three — the
pump idle in `STANDBY`, current on the ADC floor (below `IIN_CHARGING_UA`, so the current guard
cannot answer), and `Vin` at `2 · VBAT` inside the ±80 mV slack, which is exactly the veto's
condition. In the same trace the charge flag dropped on its own for about 4.2 s (`po=1, ch=0` rows)
while `POWER_ON_LINE` stayed: that is the 20 s charging hold expiring during the same idle stretch,
a charging-icon change rather than a power-source change.

The same trace is the first positive evidence for the fix suggested in the open defects: over 20
minutes of charging the hardware bit never asserted spuriously. What it does not show is the bit
across a real cable pull, which is what an immediate unplug path would rest on.

**The OS's own log says the same thing, independently of the sampler this project wrote.** The
`Microsoft-Windows-Kernel-Power` event 105 (power-source change) carries `AcOnline` in its property
bag, so the direction of each event can be read without parsing the message - which matters, because
the message text is localized and an English pattern match finds nothing on this tablet. Over the
boot of 22.09 that had been charging since 12:56:52 and was still charging at 13:47 (`mode 3`,
`Iin` 1 290 960 µA, `Vbus` 9 184 000 µV), the event log holds four `AcOnline=false` pairs and
nothing else:

| `AcOnline=false` | `AcOnline=true` | window |
| --- | --- | --- |
| 13:02:25.318 | 13:02:28.112 | 2.794 s |
| 13:17:09.871 | 13:17:12.171 | 2.300 s |
| 13:17:37.499 | 13:17:38.505 | 1.006 s |
| 13:20:24.236 | 13:20:26.281 | 2.045 s |

The last three are the three rows of the table above, at the same wall-clock times; the first fell
about two minutes before the trace started, so all four sit inside one 24-minute stretch of that
boot. A real removal writes a single `AcOnline=false` with no companion until the cable returns;
none is present, so every one of the four is a phantom on a cable that never moved, and the
charging session of that boot was uninterrupted.

What followed that stretch is the control, and it narrows the trigger: the next hour of tracing
(13:50:27-14:50:26, 8 685 samples, the pump in `mode 3` in every one, `Iin` 1.13-1.32 A, `FAULT1`
`0x00` in every one) published no transition at all - the single recorded one is the initial row -
and the OS log for the same hour holds no event 105 either. So the defect is not a background rate:
it needs the pump out of switching, which is the electrical state in every phantom row of the table
above and in none of the hour. A phantom and a real removal are therefore both rare events in any
single window, which is why the instrument has to be left running rather than sampled on demand.

### A real removal and a real insertion, measured (22.09, 14:56-14:58)

The operator pulled and reinserted the cable inside a running trace, which is the first measurement
of the two latencies reported as "several seconds". The rows are 250 ms apart; the tool fields in a
row are up to 2 s old (the tool is called every eighth tick), so the electrical edges are placed to
within that and the OS edges to within 250 ms.

| wall clock | what changed | state |
| --- | --- | --- |
| 14:56:54.8-56.8 | cable out | `mode 3 -> 1`, `Iin -> 39 120` (the floor), `Vin -> 8 864 000` (24 mV from `2 · VBAT`), `FAULT1 -> 0x30` (bit 4 **set**) |
| 14:57:02.668 | `po 1 -> 0`, `ch 1 -> 0` | published DC, about 7 s after the pull - the raw predicate went false at the pull and the delay is the 8 s `ONLINE_HOLD_MS` |
| 14:57:04.892 | `po 0 -> 1`, `ch 0 -> 1` | **phantom AC, cable still out**; `FAULT1` was `0x21` (bit 4 clear) at that moment |
| 14:57:10.433-16.127 | ADC hibernates | all channels zero, `FAULT1` `0x30`; DC published at 14:57:16.127 |
| 14:57:23.947 | input state changes | `mode 3`, `Vin` 4 928 000, `FAULT1` `0x00`, current still on the floor |
| 14:57:28.765-29.052 | `po -> 1`, then `ch -> 1` | about 5 s after that state appeared |
| 14:57:30.816 | back to idle | `mode 1`, `Vin` 8 864 000, `FAULT1` `0x30` |
| 14:57:38.842 | `po -> 0`, `ch -> 0` | published DC, exactly 8.0 s after bit 4 set |

Two things are new here. First, **bit 4 followed both real removals** (`0x30` within one sampling
interval) after being clear for all 8 685 samples of the preceding charging hour: that is the
positive evidence the immediate-unplug design was waiting for, and it is the second half of the
answer to the operator's report - a removal is recognised at the ADC in well under a second and then
held for 8 s by design. The insertion half is not a hold problem at all: the state at 14:57:23.947
was already "bus present, hardware says present" and the publication still took about 5 s.

Second, **the phantom at 14:57:04.892 is not the hold's arithmetic**. For the AC flag to come back,
`online_raw` had to answer true twice in a row, and with the values that row carries (bit 4 clear,
`Vin` 8 864 000, `VBAT` 4 420 000, current on the floor) the doubling test is *24 mV* from exact
against a slack of ±80 mV, so the veto fires and the verdict is false. The verdict therefore flipped
on a difference below two ADC steps (`LN8000_ADC_VAC_STEP` is 16 000 µV), i.e. the floating-bus
decision at that pack voltage is being made by ADC noise. The second route is the 4.6-6.0 V branch,
which tests only `vbus >= vbat + 200 mV` and asks for no current evidence at all - at
14:57:23.9-28.8 the node sat at 4 928 000-5 072 000 µV with the pump engaged and `Iin` on the floor,
and that branch publishes AC. A fix aimed only at the doubled case leaves this route open.

What the trace cannot settle is which of the two the 14:57:23.947 state was - a cable going back in
at the default 5 V, or a floating node at 5 V with no cable - because the tool's fields belong to a
different moment than the tick that decided. The mark poller (`marks.csv`, `marks_edges.log` on the
tablet) exists for that: it samples the driver's own per-tick marks - `BattPwr`, `SuMode`, `SuVinUv`,
`SuIin`, `Fault1Sts`, `VbatAdcMv`, `InputUsable`, `ConnCount` - every 250 ms, so the published flag
and the inputs that produced it come from the same tick.

Read at 15:05 with the cable out: `BattPwr = 2` (discharging), `Fault1Sts = 0x30` (bit 4 set),
`SuMode = 1`, `SuVinUv = 8 864 000`, `SuIin = 39 120`, `InputUsable = 1`, `ConnCount = 1` - the
reflection on a floating bus with the readings fully usable, which is what makes the doubling test
fire in the first place.

That poller then ran for 120 minutes with the cable out and recorded **no edge at all**: `pwr = 2`,
`mode = 1`, `Fault1Sts = 0x30`, `SuVinUv = 8 864 000` in all 24 669 samples, and the OS log holds no
event 105 for the same window. Two things follow. The driver settles correctly once the transients
are over - with no input it stays quiescent (no mode change, no retry) and the published flag is
right for the whole 2 h 10 min, which is the behaviour the operator sees as "no cable, on battery".
And bit 4 does not clear by itself while the bus is quiet: the two clearings in the table above
(14:57:03.6 and 14:57:23.9) both happened while the driver was actively re-engaging the pump
(`mode 3`, the re-elevation counter moving), which is when the input node is being driven and the
comparator can be read differently. `[inference]` That is consistent with a live comparator verdict
that is only as good as the node it is reading, and it is the reason the fix must keep the current
gate: the bit is trustworthy when it asserts on a quiet bus, which is exactly the removal case.

Two fixes follow from this measurement and are in 0.3.1 (`CHANGELOG.md`): the 4.6-6.0 V band now
requires current above the ADC floor, and a hardware unplug with no current ends the online window
on the tick it is seen instead of 8 s later. What remains open is the third route - the doubled-bus
tie-break, where the verdict sits less than two ADC steps from exact doubling (24 mV against the
80 mV slack) and a single sample can flip it. The two fixes do not touch that branch; a fix there
has to decide what a doubled bus with no current and a clear bit *means*, and the honest answer is
"no evidence either way" rather than "absent".

Evidence: `trace4.csv` and `edges4.log` next to the 0.3.0 tool copy on the tablet (250 ms rows and
an edge log written as the edges happen); the OS events `Kernel-Power 105` at 14:57:02.578,
14:57:04.843, 14:57:16.082, 14:57:27.940 and 14:57:37.996; the marks read over SSH at 15:05.
Code: `crates/ln8000/src/battery_policy.rs:142,149` (the 4.6 V threshold and the 200 mV band test),
`:28` (`IIN_CHARGING_UA`), `:19` (the 8 s window), `crates/ln8000/src/encoding.rs:243` (the 80 mV
slack), `:279-296` (the 16 mV VAC step and why a zero is not a sample).

One question an immediate-unplug fix has to answer before it is written: whether `FAULT1` bit 4 can
be read as a live verdict rather than a latch. The observed behaviour rules out a latch that only an
explicit clear can release - the trace that read `0x00` in all 346 samples ran on a boot whose
charging session began with an insertion at 12:56:52 into a tablet that had booted six minutes
earlier with the cable out, so the bit had been through a removal and a return and was back at zero
without any clear written by this driver. `[inference]` What is still unmeasured is how fast the bit
follows a real removal and whether it stays set for as long as the cable is out; that is what the
running 60-minute trace is for. The design degrades safely either way: if the bit follows within a
tick or two the hold can be released on it in about 0.25 s, and if it lags then the ADC guards carry
the first ticks exactly as they do today, because suppressing the veto only removes an argument for
"absent" while the bit is clear - it cannot invent a removal.

Evidence: the sampler and the CSV it wrote (`trace.ps1`, `trace.csv` next to the 0.3.0 tool copy on
the tablet); 346 telemetry samples, 2 766 OS-side samples, 8 published transitions in 1 199 788 ms.
The cross-check is the System event log of 22.09 read over SSH at 13:47-13:48 (`Kernel-Power` 105,
property 0 `AcOnline`, 14 events back through earlier boots), which is the OS's own record rather
than this project's sampler; the trace start is placed at 13:04:30 by arithmetic on its own rows.
Code: `crates/ln8000/src/battery_policy.rs:182-203` (the guard order), `:19` (the 8 s window),
`crates/ln8000/src/encoding.rs:263-271` (`vin_is_doubled_vbat`).

The ordering requirement that follows: **the online hold must not expire on a tick that carries no
evidence.** Concretely, the online hold is advanced with `Hold::update_evidence(Option<bool>, ...)`,
and `None` leaves the held flag, the timestamp of the last true sample and the arming run untouched;
`online_raw_with_evidence` returns `None` precisely when the input readings are unusable and the
hardware did not say the input was gone. The tests pin this: 60 s of unreadable ticks with bit 4
clear must not clear `POWER_ON_LINE`, and the evidence timestamp must not move.

One second-order note, `[inference]` from the code path rather than a measurement: the charging hold
is updated with `Hold::update`, not `update_evidence`, so an evidence-free tick *can* age it. What
keeps `CHARGING` published across a hibernation is the peak over the 5 s Iin window
(`IIN_WINDOW_MS = 5_000`); when that window rolls over during a hibernation the peak resets to the
current (zero) sample, so `CHARGING` clears about 20 s later while `POWER_ON_LINE` stays. That is a
charging-icon change, not a power-source change, so it does not by itself re-apply the display
policy.

Evidence: `crates/ln8000/src/battery_policy.rs:19` and `:22` (the two constants), `:28` and `:48`
(charging floor, arming run), `:102-118` (`Hold::update_evidence`), `:216-224` (the AC -> DC -> AC
arithmetic), `:8` (Kernel-Power 105 eleven times in twelve seconds), `:490-554` (tests:
evidence-free ticks neither clear nor arm the hold); `crates/ln8000-kmdf/src/battery.rs:479-484`
(the two holds are advanced differently in the tick), `:575-601` (`build_status`: which flags are
published);
`crates/ln8000-kmdf/src/lib.rs:2547-2566` (the tick passes `input_readings_usable`),
`:2594-2606` (the Iin window peak and its reset), `:304` (`IIN_WINDOW_MS = 5_000`),
`:2551-2553` (the backlight-policy consequence in the comment). The doubled-VBUS route:
`crates/ln8000/src/encoding.rs:243` (`VBAT_VIN_HALF_SLACK_UV = 80_000`), `:263-271`
(`vin_is_doubled_vbat`), `crates/ln8000/src/battery_policy.rs:189` (where the veto sits in the
guard order), `:430` (the test that pins it, and that a fix would have to move).

---

### The ADC was asleep when the latency fixes were deployed (22.09, 17:52-18:05)

Installing `20.47.10.666` at 17:54 to test the two fixes above produced a third finding, and it is
the one that decides whether a cable test can say anything at all.

From 17:56 the driver's own marks read `SuVinUv = 0`, `SuIin = 0`, `VbatAdcMv = 0`,
`InputUsable = 0`, `MaxIinUa = 0` with `Fault1Sts = 0x30` and `ConnCount = 1`. Zeros alone prove
nothing - they are also what a read failure looks like - so the question is whether the telemetry
tick is running at all. It is: two reads six seconds apart gave `SocAgeMs` 21 474 then 27 544, and
that mark is recomputed from the tick's own timestamp on every pass, while `PumpOpen = 1` and the
PM8150B gauge answered normally (`SocRaw = 225`, `BattPct = 88`, `SocSrc = 1`). `AdcValid = 1` puts
the zero where it belongs: bit 0 is the VBAT channel, and a connected cell cannot read 0 V, so the
LN8000 ADC is hibernating and every channel answers successfully with `0x00` (finding 2).

Why that matters is not the online verdict - `online_raw_with_evidence` gets `(false, true)` there
and answers `Some(false)`, which is right - but `input_present`, which is `vbus_uv > 0` after the
doubling and critical-fault checks. With a sleeping ADC it is false, so `engage_state` returns
`ENGAGE_NO_INPUT`, `hvdcp::input_present_from_vin(vbus)` is false, and the driver cannot see a brick
that arrives: **nothing in the driver wakes a hibernated ADC.** `ADC_CTRL` is written only inside
`Pump::configure()` - init step 9 leaves `AutoHibernate` + `Sec4` there, and the NTC threshold write
shares the register - and `configure()` runs at device start plus the shutdown-recovery paths, which
are reached only from a live input sample. The hardware's VBUS comparator (`FAULT1` bit 4) keeps
working while the ADC sleeps, and until 0.3.1 it was wired to the battery flags only.

The contrast is in the same file, immediately before the install. From 15:01:03 to 17:52:00 the poll
recorded `vin = 8 864 000`, `iin = 39 120`, `f1 = 0x30`, `vbat = 4 420`, `usable = 1`, `maxiin = 39 120`,
`winout = 1`, `reel = 1` - samples live and the node at `2 · VBAT` (2 × 4 432 mV ≈ 8 864 mV), which is
the reflection of finding 3 with the cable out, not an adapter. `online_raw` rejects it twice over
(bit 4 first, then `vin_is_doubled_vbat`), so nothing was published for it; the interesting part is
that this reflection reads *above* `VBUS_ELEVATED_UV = 6 V`, so the elevated branch on its own would
have accepted it as an adapter. A caution on the poll's own evidence: its edge predicate watches
`pwr`, `mode`, `f1` and `conn` only, so "no edges for three hours" does not prove `vin` and `iin` were
static - the CSV rows at 17:02, 17:45 and 17:52 are identical to the last digit, which is what the
claim rests on.

Evidence: the 250 ms mark poll (`marks.csv`, `marks_edges.log` in a working directory on
the tablet, started 15:01:03 for 120 min and again 17:02:13 for 240 min); the live read at 18:05
(`AdcValid`, `SocAgeMs`, `SocRaw`, `BattPct`, `SocSrc`, `PumpOpen`, `SuVinUv`, `SuIin`, `SuMode`,
`ConnCount`, `ReElevateN`, `SysSts`, `SafetySts`, `VoltQual`, `ChargeAttemptN`); the installed image
is `ln8000_kmdf.inf_arm64_603368707346a412` in the tablet's DriverStore with the same `ln8000_kmdf.sys`
sha256 as the local build (`57fe29384d0a1af9…`), so the marks belong to this build. Code:
`crates/ln8000-kmdf/src/lib.rs:2510-2521` (the four channel reads), `:2557`
(`input_readings_usable`), `:2544` (`input_present`), `:2284-2296` (`engage_state`),
`crates/ln8000/src/driver.rs:425` (`configure`) and `:491-508` (init step 9),
`crates/ln8000/src/battery_policy.rs` (`adc_wake_needed` and its test).

---

### A working 5 V brick that never draws current, and the two fixes it invalidated (22.09, 20:34)

The operator attached a 5 V supply at 20:34 with `20.47.10.667` installed, reported that the
tray showed nothing while the meter showed current being drawn, and the 250 ms poller recorded
the whole session. It is the measurement that decides what the 4.6-6.0 V band and the unplug
release are allowed to assume.

The poller's first non-zero row after two hours of hibernation is 20:34:16.678:
`pwr = 2`, `mode = 1`, `f1 = 0x21` (bit 4 clear), `vin = 5 056 000`, `iin = 39 120`,
`usable = 1`, `awake = 0`, `adcvalid = 0`. From there to 20:36:12 the node oscillates:
`vin` runs 0 -> 5 056 000 -> 4 384 000 -> 4 416 000 -> 0 -> 4 912 000 -> 4 480 000 -> 4 464 000
-> 5 056 000 -> 4 528 000 -> 4 880 000 -> 4 544 000 -> 5 056 000 -> 4 464 000, spending most of
its time in **4.38-4.55 V**, i.e. below the 4.6 V floor, while `f1` cycles `0x30` -> `0x21` ->
`0x00` and `iin` never leaves the 39 120 uA ADC floor. `vbat` is a credible cell throughout
(`4 055 000-4 070 000`), `conn = 1`, and `pwr` is **2 for every single sample** - no AC, no
charging flag, for the whole session.

Two of the fixes in 0.3.1 are what produced that silence, and both for the same reason: with a
brick that never transfers current, "no current" is not evidence about the brick.

1. The 4.6-6.0 V band asked for current above the floor. The windows where `vin` reaches
   5 056 000 are exactly the windows that would have answered "online" before; with the gate
   they answered "no adapter". Below 4.6 V the band does not answer either way (that floor is
   older than the gate), so the flag had no route left.
2. The unplug release fired on the first tick with bit 4 set and no current. The edge log gives
   the duration of those windows: 20:34:33.379 -> 20:34:33.669 (**290 ms**) and
   20:34:46.225 -> 20:34:46.512 (**287 ms**) - one tick each, every 15-40 s, created by the
   driver's own engagement pulses collapsing the adapter's output. Each one released
   `POWER_ON_LINE`, and nothing re-armed it: the current branch needs `Iin >= 80 000`, and the
   pump never gets there.

That last number is the second finding, and it is independent of the flag: `ChargeAttemptN = 12`
and `LastEnableErr = -4` (`pump_error_code`: `ModeNotReached`) - twelve attempts, the chip never
reached the requested mode, so no current flowed and the 5 V brick charged the tablet through
whatever path the stock stack drives, not through this pump. A 5 V input cannot run the 2:1
stage (`2*Vbat + 200 mV` is 8.31 V at this pack voltage), so the attempts are aimed at a mode
the input cannot support; that is a separate defect and is left open here.

What was done about it, and what it cost: the current gate in the band is **reverted**
(voltage-only again, with `an_idle_five_volt_brick_is_online_even_with_no_current` and
`the_five_volt_band_still_rejects_the_doubled_bus` pinning both sides), and the unplug release
now requires the bit to hold for two consecutive ticks (`battery_policy::UNPLUG_RELEASE_RUN`),
which a real removal satisfies - the bit asserted at 14:57:30 and stayed asserted for the 2 h
51 min of the 15:01-17:52 poll - while a 290 ms window never does. Removal latency goes from
0.25 s to 0.5 s; the phantom route through the 4.6-6.0 V band **stays open**.

Why it stays open, stated plainly: in that band a 5 V brick with the pump out of transfer and a
5 V node with nothing behind it read the same in every field this tick has - `vin` (4.38-5.06 V
in both), `vbat` (a credible cell in both: 4 055 000 here, 4 420 000 at 14:57), `iin` (the
39 120 floor in both) and bit 4 (clear in the windows of both). The only differences are
dynamic: the real brick's node responds to the driver's own pulses with swings down to 0 V,
and the engagement attempts move it, while the floating node sat still. A fix therefore has to
be a load test - ask the pump to draw and see whether the node holds - and that is a design
decision, not a threshold.

One more thing the session settles: the ADC wakes on VBUS by itself. At 20:34:16 the samples
were already usable (`usable = 1`, `adcvalid = 0`) with `awake = 0`, before the comparator wake
path wrote anything (`AdcWakeN` went 0 -> 1 at 20:34:35, and the mode was returned to
`AutoHibernate` at 20:34:46 with the samples staying usable afterwards). The wake path added in
0.3.1 is therefore a safety net for the case where nothing else wakes the chip, not the cure
for the invisibility that was suspected at 18:32.

Evidence: `marks667.csv` and `marks667_edges.log` next to the tool copy on the tablet (250 ms
rows from 18:32:50, edge log with the window durations above); the marks read at 20:36
(`pwr = 2`, `vin = 4 448 000`, `iin = 39 120`, `f1 = 0x21`, `usable = 1`, `awake = 0`,
`waken = 1`, `adcvalid = 0`, `maxiin = 39 120`, `att = 12`, `lasterr = 4294967292`).
Code: `crates/ln8000/src/battery_policy.rs` (`online_raw`'s band and its doc comment,
`unplug_release`, `UNPLUG_RELEASE_RUN`, the tests named above),
`crates/ln8000-kmdf/src/battery.rs` (the release site and the comment that records both
guards), `crates/ln8000-kmdf/src/lib.rs:4379-4393` (`pump_error_code`, where `-4` is
`ModeNotReached`).

---

### The 4.6 V floor, the fuel counter's sign, and what the tray was really missing (22.09, 20:50-21:40)

The operator's report was "it does not show charging at all, while the meter shows current being
drawn". The measurement says he was right about the current and the tray was wrong about the
adapter, and it produced two reversals of fixes made earlier the same day.

**The floor, not the phantom.** With the brick attached and the platform's own SMB5 buck (PM8150B)
carrying the charge, the LN8000's input node sat at `SuVinUv = 4 400 000-4 544 000` with the cell at
`4 130 000-4 180 000` and `SuIin` on the 39 120 uA ADC floor, because the pump never reaches its mode
on a 5 V brick (`LastEnableErr = -4`, `ModeNotReached`; `ChargeAttemptN` reached 21 in one session).
`FAULT1` bit 4 was **clear** - the hardware comparator, whose asserted state is what the removal path
trusts, saying the bus is present. `BattPwr` still read `2` for the whole session, and the readings
show why: `online_raw` ended in `if vbus_uv < VBUS_ONLINE_UV { return false }`, so a node below
4.6 V was rejected before the headroom test over the cell was ever reached. The tray was not missing
the current, it was missing the adapter, and Windows applied the DC idle policy to a tablet on a
brick - the same failure the floor was introduced to prevent, reached from the other side.

The floor is therefore gone (`battery_policy::online_raw`): below `VBUS_ELEVATED_UV` the decision is
the 4.2 V floor plus `vbus >= vbat + 200 mV`, with the comparator having already spoken above. The
guard the floor used to provide is not needed in this band, and that is a measurement rather than an
argument: an unplugged node does not sit just above the cell on this board, it reads `2 · VBAT`
(8 464 000 uV at a 4 232 000 uV cell; 8 864 000 uV at 4 432 000 uV), and the doubling veto
(`encoding::vin_is_doubled_vbat`) catches that before the band is reached. What the change does cost
is the 4.6 V floor's protection against a *phantom* node in the 4.2-4.6 V band with bit 4 clear; no
such reading has been observed, and the phantom that has been observed (4.93-5.07 V with the pump
engaged) lives above this band and is unaffected.

**The charging flag had no witness.** `CHARGING` is `charging_raw(online, iin, peak)` and `iin` is the
pump's own input current, so on this brick it could never be true - the current was real (the operator's
meter, the gauge counter climbing `199 -> 220 -> 233`, the cell `4 145 -> 4 183 mV`) and invisible to
the only sensor the flag consulted.

The obvious replacement is the fuel counter's cell current (`FgIbatUa`, `spb.rs::ChargeRegs::ibatt_ua`),
whose field documentation says negative is current into the cell. **The measurement says its sign does
not discriminate the two directions on this board**, so it was rejected for that role:

| state | cell current | SOC | cell voltage | what the pack was doing |
| --- | --- | --- | --- | --- |
| pump in 2:1, 8.9 V at the input, `Iin` 0.90-1.19 A | `FgIbatUa = -3 523 435` | 223 -> 234 (rising) | 4 145 -> 4 183 mV | charging at ~3.5 A |
| cable out, `FAULT1 = 0x30`, `Vin = 2 · VBAT`, pump idle | `FgIbatUa = -537 109` | 234 -> 233 (falling) | 4 231 -> 4 221 mV | discharging at ~0.54 A |

Negative in both states, and the magnitude matched the cell current in both. The same field read
`+786 132` (positive) in a third, also-discharging state after a driver reinstall, so the sign is not
merely inverted - it is not usable as a direction at all. What *did* answer correctly in every state is
the counter's own SOC: it rose while the pack filled and fell while it drained. The direction therefore
comes from `battery_policy::SocTrend` (a rise is remembered for `SOC_RISE_HOLD_MS`, a fall clears the
verdict on the spot), and the cell current stays a diagnostic mark.

**The 20:34 pulse windows and the run length.** The removal release, which had been `2` ticks since
the morning, is now `3` (`UNPLUG_RELEASE_RUN`). The measured windows lasted 290 ms and 287 ms against a
250 ms telemetry tick, so a single window can contain two consecutive samples - two ticks are not a
safe threshold. Three ticks span 750 ms, which no measured window reaches; the cost of a real removal
is 750 ms, against the 7-8 s the plain window took before this path existed. A pulse longer than that is
still covered, because the release is not the end of it: the SOC trend re-arms the flag within
`HOLD_ARM_RUN` ticks.

Live after the 20.47.10.671 install, with the pump in 2:1 on the operator's brick:
`pwr = 5` (`POWER_ON_LINE | CHARGING`) stable over 40 s, `mode = 3`, `Vin = 8 832 000-8 912 000`,
`Iin = 904 650-1 193 160`, `SocRaw` `223 -> 224`, `GaugeCharging`-era marks replaced by
`SocRising = 1`. With the cable out: `pwr = 2`, `f1 = 0x30`, `Vin = 8 464 000` (doubled), `SocRising = 0`,
`AdcAwake = 0` and `AdcValid = 1` (the ADC asleep with nothing to measure, as designed).

Evidence: the marks read over SSH at 21:0x-21:40; the 250 ms poller writing `marks671.csv` and
`marks671_edges.log` on the tablet (started 21:35:22, 240 minutes). Code:
`crates/ln8000/src/battery_policy.rs` (`online_raw`, `charging_raw`, `SocTrend`,
`UNPLUG_RELEASE_RUN`), `crates/ln8000-kmdf/src/battery.rs` (`update_from_telemetry`,
`set_gauge_raw`), `crates/ln8000-kmdf/src/lib.rs` (the `SocRising`/`SocRiseAgeMs` marks).

## 6. The 2:1 switching window is derived from the pack voltage, not fixed

A 2:1 charge pump passes real power only while the input sits in a narrow band above twice the pack
voltage, and that band moves as the pack charges. This project derives everything from VBAT
(`crates/ln8000/src/encoding.rs`):

| Constant | Value | Function | Result |
|---|---|---|---|
| `SWITCHING_HEADROOM_UV` | 250 mV | `min_vin_for_switching_uv` | admission: `2*vbat + 250 mV` |
| `SWITCHING_WINDOW_FLOOR_UV` | 200 mV | `window_floor_uv(vbat)` | band floor: `2*vbat + 200 mV` |
| `SWITCHING_WINDOW_TOP_UV` | 400 mV | `window_top_uv(vbat)` | band top: `2*vbat + 400 mV` |
| `SWITCHING_WINDOW_TARGET_UV` | 300 mV | `window_target_uv(vbat)` | bus target: `2*vbat + 300 mV` |
| `SWITCHING_MIN_VIN_UV` | 8.0 V | - | absolute minimum input for 2:1 |

`vin_in_switching_window(vin, vbat)` tests the closed band and returns false for a non-positive Vin
or an unknown pack. Below the absolute 8.0 V floor 2:1 is never requested and the 1:1 bypass is the
only elevated path; the mode chooser never selects 1:1 at or above 8.0 V, because that would put the
input straight across the cell.

How the band was derived: four measured points on 18.09 with the pack at 4.40-4.44 V. `9.088 V ->
1887 mA` and `9.280 V -> 2513 mA` carry power; `9.744 V -> 39 mA` and `9.888 V -> 39 mA` are the
case that matters - the chip still reports mode 3 (`SYS_STS = 0x04`) while carrying only the 39 mA
ADC floor (8 * 4.89 mA), and below the floor the mode is refused outright. Those points bracket the
band at roughly `2*Vbat + 200 mV` to `2*Vbat + 400 mV`. The target is the centre rather than an
edge because one QC3 step is 200 mV, i.e. as wide as the band: aiming at an edge means a single
pulse leaves the band, and leaving it *upward* stops the transfer.

Why a fixed floor breaks the system: at a pack voltage of 4.42 V the band top is 9.24 V, three QC3
steps *below* the 9.5 V this driver used to hold. With the old fixed target the bus sat at 9.888 V
against a 9.04-9.24 V band - mode 3 at the 39 mA floor, about 0.38 W where 2:1 delivers 16-23 W on
this platform. Worse, the telemetry tick judged that working point "outside the window"
(`outside = !vin_in_switching_window(vbus, vbat)`) and the correction logic then moved the bus, so
the driver's own telemetry killed the 2:1 state it had just been handed. The 9.5 V value survives
only as a fallback for the case where VBAT could not be read at all
(`PUMP_VIN_TARGET_MIN_UV`), together with an 8.0 V absolute floor and a 9.6 V ceiling that keeps the
target reachable within the pulse budget.

Evidence: `crates/ln8000/src/encoding.rs:86-177` (constants and functions, with the measured points
in the comments), `:330-348` (mode selection from Vin *and* Vbat), `:685-729` (tests: the derived
target admits 2:1 across the pack range, and the old fixed 9.5 V floor is outside the band at
4.42 V); `crates/ln8000-kmdf/src/hvdcp.rs:209-266` (`PUMP_VIN_TARGET_MIN_UV` = 9.5 V fallback, 8.0 V
absolute floor, 9.6 V ceiling, compile-time assertions), `:445-504` (`target_vbus_uv`,
`trim_target_uv`, `window_floor_uv`), `:1662-1718` (`nudge_vin_into_window`: three cases);
`crates/ln8000-kmdf/src/lib.rs:3078-3189` (the tick that nudges the bus and the 18.09/19.09
measurements in the comments); `crates/ln8000/tests/thermal.rs:1-13,214-246` (a raised Vin never
falls back to 1:1; at 5 V it does).

---

## Open defects and unknowns

These are unresolved in this project. Each carries its evidence, and where something is only partly
understood that is said explicitly.

1. **The SPMI bus response layout is not confirmed by reverse engineering.** Reads that go through
   the parent route return zeros because the address is not passed, and the byte layout of
   `IOCTL_RESOURCE_HUB_TRANSACT` (0x32C004) was not established; the working reads in this driver go
   through `\Device\Spmi\SUPERUSER` instead. A port that has to build its own SPMI transport will
   hit this first.
   Evidence: `docs/REGISTERS.md:63-69`; `docs/HANDOVER.md:139`; `docs/STATE-2026-09-17.md:24,29-36`
   (the parent route accepts requests but returns zeros).

2. **Two earlier SPMI probe results contradict the later one and the difference is not
   understood.** Eight access masks failed with `0xC0000001` on an early build; the same object
   opened with `STATUS_SUCCESS` on build 20.47.10.605 and carried a full APSD + QC3 cycle. The
   project states it does not know the cause of the earlier failure.
   Evidence: `docs/STATE-2026-09-17.md:835-852` and its correction at `:900-920`.

3. **The LN8000 VBAT channel reads low against the fuel gauge.** A later measurement record puts the
   difference at 42-43 mV (`BattVbat = 4375` against `FgVbattMv = 4315`). That channel feeds the
   2:1 gate and the taper, so the offset moves the band with it. The two readings are published side
   by side for exactly this comparison (`FgVbattMv` from the PM8150B fuel counter, `VbatAdcMv` from
   the pump ADC); this project did not resolve the offset.
   Evidence: `crates/ln8000-kmdf/src/lib.rs:2746-2757,2867-2879` (both marks and why they exist);
   the measured numbers come from the project's measurement record for the 19.09-21.09 sessions, not
   from a line of code. Note that a much larger divergence is expected and documented during 2:1
   switching, where VBAT reads approximately `Vin / 2` - that one is understood, the 42-43 mV is
   not.

4. **`AdcValid` bit 1 (die temperature) is reported valid while the channel is hibernating, so a
   die temperature of 160.0 °C is published.** The flag is computed from the read result alone
   (`temp_read.is_ok()`), with no plausibility check, while a hibernated channel answers
   successfully with zero. Raw code 0 decodes to +160.0 °C (the clamp), and that value is published
   in the `DieTempDc` mark and in the status IOCTL response. Protection is not affected - the guard
   gates every temperature decision on `die_temp_usable`, which rejects anything above +125.0 °C -
   so this is a reporting defect, but it will mislead anyone watching the temperature during an
   idle pump.
   Evidence: `crates/ln8000-kmdf/src/lib.rs:2542-2543,2581-2588,2996-3004` (flag, `AdcValid` bits
   and the published mark); `crates/ln8000-kmdf/src/ioctl.rs:91,206` (the field in both status
   structures); `crates/ln8000/src/guard.rs:24-31,349-351` (the plausibility gate);
   `crates/ln8000/src/encoding.rs:279-296` (why a successful zero is not a sample).

5. **`vin_is_doubled_vbat` can veto a genuinely attached input (a false negative of the online
   decision).** A real 6-10 V brick with the pump idle (Iin at the 39 mA floor) and a pack that
   happens to sit near `Vin / 2` matches the reflection test and is dropped by the veto, so
   `POWER_ON_LINE` is not set while the brick is attached. Current evidence overrides it (the first
   branch of `online_raw`), so this only bites when no current flows. The slack is ±80 mV, and the
   predicate cannot tell a pack at half the bus from a real reflection.
   Evidence: `crates/ln8000/src/encoding.rs:238-280`; `crates/ln8000/src/battery_policy.rs:182-203`.
   The mechanism is recorded in the project's measurement note for the 19.09-21.09 sessions, and it
   was captured again with an OS-side sampler on 22.09 — three `AC -> DC -> AC` publishes to Windows
   in 20 minutes with the cable untouched and `FAULT1` bit 4 clear throughout. See finding 5 for the
   timestamps and the electrical state.

6. **An `EngageState` mark can disagree with the live mode.** `engage_state` returns
   `ENGAGE_NO_HEADROOM` (4) whenever Vin is elevated and `charge_mode` finds no headroom, without
   checking the mode the chip actually reports; `SuMode` next to it reports the chip answer. The
   result is a mark that says "elevated but not passing" while the mode reads 3. It is a mark-only
   inconsistency (no control path uses it), but a post-mortem has to know which of the two to trust.
   Evidence: `crates/ln8000-kmdf/src/lib.rs:2276-2291` (`engage_state`), `:2575` (`SuMode`).

7. **The PMIC TCC device `ACPI\QCOM0582` was seen in an Error state, and `WUDFRd` fails to load for
   the sensor platform `ACPI\QCOM059F`.** Both were seen in a 20-21.09 PnP/event-log enumeration
   (the sensor-platform failure appeared 48 times as Kernel-PnP ID 219). Neither is explained, and
   nothing here links them to charging. Note a conflict: an earlier capture (17.09) lists both
   devices as OK with `WUDFRd` as their service, so the state differs between captures.
   Evidence: this repository records the Type-C port re-initialisation attempt for `QCOM057D` and
   `QCOM0582` (`docs/COMPAT-MATRIX.md:113-115`) and the general "Problem is not 0" case in
   `docs/OPERATOR-CHECKLIST.md:127`; the Error-state and ID 219 records themselves come from the
   project's 20-21.09 measurement records, not from this tree.

8. **The maximum charge rate was never measured with a reliable instrument.** Rate was estimated
   from the battery percentage over time (68 % -> 69 % in 10 min 7 s), which cannot show whether the
   hardware maximum is reached; an external USB wattmeter and a known PD supply are needed. This is
   recorded in the project as an open acceptance item rather than a result.
   Evidence: `docs/STATE-2026-09-17.md:553-573`; `docs/COMPAT-MATRIX.md:43-49` (the "remains
   untested" table).

10. **The fuel counter's cell current carries a usable magnitude but not a usable sign.** The
    field is decoded exactly as the vendor decodes it (`spb.rs::read_charge_regs`, sign-extended
    from bit 15 of the `0x41A2`/`0x41A3` pair, 488.281 uA per step), and its magnitude matched the
    cell current in every state measured on 22.09 - but the sign did not follow the direction: it
    read negative while the pump pushed ~3.5 A into a pack whose SOC climbed, negative again while
    the pack drained at 0.54 A with the cable out and the SOC falling, and positive (786 132 uA) in
    a third, also-discharging state after a driver reinstall. The project therefore does not use it
    as a direction witness (the SOC trend does that job) and does not know why the sign behaves
    this way. A misaligned register pair is one hypothesis, but a byte-order error would move the
    magnitude by 256x and the magnitudes are right, so the hypothesis does not fit on its own.
    Evidence: the marks read over SSH at 21:0x-21:40 on 22.09 (`FgIbatUa`, `SocRaw`, `FgVbattMv`,
    `VbatAdcMv` read together in one snapshot); the table in the section above;
    `crates/ln8000-kmdf/src/spb.rs` (`ChargeRegs::ibatt_ua` and its decoder),
    `crates/ln8000/src/battery_policy.rs` (`SocTrend`).

11. **The pump never reaches a mode on a 5 V brick, and why is not established.** With
    `Vin = 4 400 000-4 544 000` and a cell at ~4.15 V the driver asked twelve then twenty-one times
    and every attempt ended in `LastEnableErr = -4` (`pump_error_code` -> `PumpError::ModeNotReached`):
    the chip never reported the requested operating mode, so no current ever flowed through the pump
    while the platform buck charged the pack. `charge_mode(vin, vbat)` at those voltages cannot offer
    `Switching` (it needs `2 · Vbat + 200 mV`, about 8.5 V) and the attempts therefore aim at
    `Bypass`; whether the chip refuses `1:1` below some input level, or the enable sequence is wrong
    for it, was not determined. The 21:35 session on the same brick at `Vin = 8.9 V` reached
    `Switching` on the first attempt (`ChargeAttemptN = 1`), so the failure is specific to the 5 V
    regime. Nothing in the presence decision depends on it any more - that is what the fuel counter
    and the low band are for - but a driver that cannot use a 5 V brick's current is leaving the
    platform's own buck to do the work.
    Evidence: `ChargeAttemptN`, `LastEnableErr`, `SuMode`, `SuVinUv`, `SuIin` in the 22.09 marks;
    `crates/ln8000/src/encoding.rs` (`charge_mode`, `min_vin_for_switching_uv`),
    `crates/ln8000-kmdf/src/lib.rs` (`engage_state`, the attempt path).

9. **Charging with a PD supply: AC is reported, capacity growth is not demonstrated.** The record
   lists a 25 W PD supply with "0 % in 1800 s" and a second PD supply with "+1 % in 60 s", while the
   same section notes that the percentage instrument was later found unreliable and that only the
   pump ADC and battery voltage could be trusted. Until this is settled with a wattmeter, treat the
   fast-charge power statements as unverified.
   Evidence: `docs/COMPAT-MATRIX.md:29-33,118-122`; `docs/COMPAT-MATRIX.md:136-143` (the correction
   about the percentage signal).

---

## What a reader can do with this

1. **See the hibernation directly.** Read `ADC_CTRL` (0x23): bits 5:7 = 0 with bits 3:4 = 3 is
   `AutoHibernate` + `Sec4`, the state a configured LN8000 is in (the value reads 0x1C when the NTC
   high bits are zero). Then read the channel registers 0x09..0x12. All zeros plus that `ADC_CTRL`
   is a sleeping ADC, not a dead input.

2. **Tell a hibernated zero from a real one.** A real VBAT raw code is never 0 on a connected cell
   (3.0 V is code 600 at 5 mV/LSB), and a real die-temperature code is never 0 (+160.0 °C). If VBAT
   and the other channels are all exactly 0 while ADC_CTRL says AutoHibernate, the chip is asleep;
   wake it by writing `ADC_CTRL` bits 5:7 = 6 (`Normal`) or by making the pump work, then re-read.
   Never feed a zero into a power decision: on this hardware that is what turns "no sample" into
   "the adapter was unplugged".

3. **When a charge pump appears to stop charging, read three things before suspecting the cable.**
   `SYS_STS` (0x03): mode 3 with an input current at the 39 mA floor means the pump is engaged but
   transferring nothing. `FAULT2` (0x06): bit 7 is the `IIN_OC` latch, which on this tablet latches
   on the first entry into 2:1 unless the protection is disabled. `FAULT1` (0x05): 0x21 or 0x30 with
   `SYS_STS` bit 6 (`VFLOAT_LOOP`) set means the charge-voltage limit was reached, not a supply
   fault.

4. **Compute the bus window from a fresh VBAT and never from a constant.** Floor
   `2*VBAT + 200 mV`, target `2*VBAT + 300 mV`, top `2*VBAT + 400 mV`, admission gate
   `2*VBAT + 250 mV`, absolute minimum 8.0 V. One QC3 step is 200 mV, i.e. the whole band, so
   correct by one step and re-read. Do not hold 9.5 V: on a pack above roughly 4.42 V that is above
   the band, and the pump will report mode 3 while carrying nothing.

5. **Make the AC-online verdict evidence-gated on the Windows side.** Keep a hold on the AC flag,
   and advance it only on ticks that carry a usable reading or the hardware unplug bit. If a tick
   has neither, leave the flag (and its timestamp) untouched. Otherwise a few seconds of idle ADC
   will publish AC -> DC -> AC and Windows will re-apply the power and brightness policy while the
   cable never moves; the count of `Microsoft-Windows-Kernel-Power` ID 105 events is the metric to
   watch.

6. **Do not expect a stock Windows stack to charge from a USB-A/QC supply on this platform.** The
   detection result exists and is published; nothing reads it. Either port a consumer (the `core`
   crate here is one) or arrange the power path so that charging does not depend on the input
   current limit being raised from the adapter type.
