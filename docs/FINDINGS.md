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

Combined with finding 2, the failure mode is arithmetic: a pump-idle stretch longer than the ADC
hibernation (about 4 s) plus the 8 s online window publishes AC -> DC -> AC to Windows while the
cable never moved. On this platform that is not cosmetic: a power-source change makes Windows
re-apply the power policy, including the display brightness policy, which is how the bug was
noticed by the operator. The measured size of the symptom is in the code comment that motivated the
fix: Kernel-Power 105 (power source change) fired 11 times in 12 seconds.

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
`:2551-2553` (the backlight-policy consequence in the comment).

---

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
   The mechanism is recorded in the project's measurement note for the 19.09-21.09 sessions; it was
   not reproduced as a captured event.

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
