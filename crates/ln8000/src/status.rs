//! Parsing of the LN8000 status: mode, protections, faults and ADC readings.

use crate::encoding::OpMode;
use crate::regs;

/// LN8000 ADC channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdcChannel {
    /// Output voltage (5 mV/LSB).
    Vout,
    /// Input voltage (16 mV/LSB).
    Vin,
    /// Battery voltage (5 mV/LSB).
    Vbat,
    /// Adapter voltage (16 mV/LSB, 5 LSB offset).
    Vac,
    /// Input current (4.89 mA/LSB).
    Iin,
    /// Die temperature (0.435 °C/LSB, −25 °C offset).
    DieTemp,
    /// Battery thermistor (2.933 mV/LSB).
    TsBat,
    /// Bus thermistor (2.933 mV/LSB).
    TsBus,
}

impl AdcChannel {
    /// Channel result register.
    ///
    /// The mapping is taken from `switch (ch)` in `ln8000_get_adc_data()`:
    /// VOUT ← ADC04, VIN ← ADC03, VBAT ← ADC06, VAC ← ADC02, IIN ← ADC01,
    /// DIETEMP ← ADC07, TSBAT ← ADC08, TSBUS ← ADC09. The code occupies two bytes,
    /// so the channels are read as a pair from their own register.
    #[must_use]
    pub const fn register(self) -> u8 {
        regs::ADC_FIRST_STS.saturating_add(match self {
            // Channel numbers as in the reference driver (`enum ln8000_adc_channel_index`):
            // VOUT=1, VIN=2, VBAT=3, VAC=4, IIN=5, DIETEMP=6, TSBAT=7, TSBUS=8.
            // The ADC05 register (0x0D) is not used by any channel, so a simple
            // offset from ADC01 will not do: VBAT and the rest sit one
            // register further on.
            Self::Iin => 0,
            Self::Vac => 1,
            Self::Vin => 2,
            Self::Vout => 3,
            Self::Vbat => 5,
            Self::DieTemp => 6,
            Self::TsBat => 7,
            Self::TsBus => 8,
        })
    }

    /// Channel code, as in the driver (`enum ln8000_adc_channel_index`).
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Vout => 1,
            Self::Vin => 2,
            Self::Vbat => 3,
            Self::Vac => 4,
            Self::Iin => 5,
            Self::DieTemp => 6,
            Self::TsBat => 7,
            Self::TsBus => 8,
        }
    }

    /// Channel name for the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Vout => "vout",
            Self::Vin => "vin",
            Self::Vbat => "vbat",
            Self::Vac => "vac",
            Self::Iin => "iin",
            Self::DieTemp => "die_temp",
            Self::TsBat => "ts_bat",
            Self::TsBus => "ts_bus",
        }
    }

    /// All channels in the driver's enumeration order.
    pub const ALL: [Self; 8] = [
        Self::Vout,
        Self::Vin,
        Self::Vbat,
        Self::Vac,
        Self::Iin,
        Self::DieTemp,
        Self::TsBat,
        Self::TsBus,
    ];

    /// "channel → ADC register number" mapping, as in the driver.
    #[must_use]
    pub const fn adc_index(self) -> u8 {
        match self {
            Self::Iin => 1,
            Self::Vac => 2,
            Self::Vin => 3,
            Self::Vout => 4,
            Self::Vbat => 6,
            Self::DieTemp => 7,
            Self::TsBat => 8,
            Self::TsBus => 9,
        }
    }

    /// Converts a register code into a physical quantity.
    ///
    /// The code is 10-bit (the ADC result takes two registers), so it is taken as
    /// `u16`. Returns microvolts, microamps or tenths of a degree - depending
    /// on the channel (see [`AdcChannel`]).
    ///
    /// Arithmetic without saturation is acceptable: the code never exceeds 1023
    /// and the largest multiplier is 16 000, so a maximum of ≈ 16.4 million fits
    /// into `i32` with a huge margin to spare.
    #[must_use]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn decode(self, raw: u16) -> i32 {
        // The channel value is a 10-bit code packed into the common bit stream
        // of all channels: 8 channels of 10 bits = 80 bits = exactly 10 bytes
        // (0x09..0x12). The byte pair is taken from the channel address, but it
        // cannot be taken whole: the code is assembled only from the bits of its
        // own channel. The slicing repeats the reference `ln8000_convert_adc_code`.
        let low = i32::from(raw & 0x00FF);
        let high = i32::from((raw >> 8) & 0x00FF);
        let code = match self {
            // Bits 0..9 (IIN) and 40..49 (VBAT): the channel byte plus 2 high bits.
            Self::Iin | Self::Vbat => (high & 0x03) * 256 + low,
            // Bits 10..19 (VAC) and 50..59 (DIETEMP): 6 low bits and 4 high.
            Self::Vac | Self::DieTemp => (high & 0x0F) * 64 + (low & 0xFC) / 4,
            // Bits 20..29 (VIN) and 60..69 (TSBAT): 4 low bits and 6 high.
            Self::Vin | Self::TsBat => (high & 0x3F) * 16 + (low & 0xF0) / 16,
            // Bits 30..39 (VOUT) and 70..79 (TSBUS): 2 low bits and the high bit.
            Self::Vout | Self::TsBus => (high & 0xFF) * 4 + (low & 0xC0) / 64,
        };
        match self {
            Self::Vin => code * 16_000,
            // Android `ln8000_convert_adc_code`: `adc_raw * LN8000_ADC_VBAT_STEP`.
            // `LN8000_ADC_VBAT_MIN` (1 V) is a validity threshold, not an additive offset.
            Self::Vout | Self::Vbat => code * 5_000,
            Self::Vac => (code + 5) * 16_000,
            Self::Iin => code * 4_890,
            // As in the reference: (935 - raw) * 4350 / 1000, clamped to [-250; 1600].
            Self::DieTemp => {
                let dc = (935 - code) * 4_350 / 1_000;
                dc.clamp(-250, 1_600)
            }
            Self::TsBat | Self::TsBus => code * 2_933,
        }
    }

    /// Unit of the value returned by [`AdcChannel::decode`].
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Vout | Self::Vin | Self::Vbat | Self::Vac | Self::TsBat | Self::TsBus => "uV",
            Self::Iin => "uA",
            Self::DieTemp => "dC",
        }
    }
}

/// Snapshot of the device state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Raw value of `SYS_STS`.
    pub sys_sts: u8,
    /// Decoded mode.
    pub op_mode: OpMode,
    /// Raw value of `SAFETY_STS`.
    pub safety_sts: u8,
    /// Raw value of `FAULT1_STS`.
    pub fault1_sts: u8,
    /// Raw value of `FAULT2_STS`.
    pub fault2_sts: u8,
    /// Raw value of `LDO_STS`.
    pub ldo_sts: u8,
}

impl Status {
    /// Whether the input current limit loop is active.
    #[must_use]
    pub const fn iin_loop_active(&self) -> bool {
        self.sys_sts & regs::SYS_STS_IIN_LOOP != 0
    }

    /// Whether the charge voltage regulation loop is active.
    #[must_use]
    pub const fn vfloat_loop_active(&self) -> bool {
        self.sys_sts & regs::SYS_STS_VFLOAT_LOOP != 0
    }

    /// Charge complete.
    #[must_use]
    pub const fn charge_terminated(&self) -> bool {
        self.ldo_sts & regs::LDO_CHARGE_TERM != 0
    }

    /// Recharge required.
    #[must_use]
    pub const fn recharge_requested(&self) -> bool {
        self.ldo_sts & regs::LDO_RECHARGE != 0
    }

    /// Watchdog timer expired.
    #[must_use]
    pub const fn watchdog_expired(&self) -> bool {
        self.fault1_sts & regs::FAULT1_WATCHDOG != 0
    }

    /// Vendor "input valid" flag (`ln8000_check_status`, `.c:592-604`).
    ///
    /// The first stage is the whole [`regs::FAULT1_VFAULTS_MASK`] group in `FAULT1`;
    /// the second is [`regs::FAULT2_VOLT_FAULT`], and only when the first is clean
    /// and charging is enabled. A live `FAULT1=0x21` gives `false` already at the
    /// first stage, which is why [`Self::has_critical_fault`] (it looks only at
    /// named bits) says nothing about such an input. We publish both signals so
    /// that "no faults" is not read as "input is valid".
    #[must_use]
    pub const fn volt_qual(&self, charge_enabled: bool) -> bool {
        if self.fault1_sts & regs::FAULT1_VFAULTS_MASK != 0 {
            return false;
        }
        !(charge_enabled && (self.fault2_sts & regs::FAULT2_VOLT_FAULT) != 0)
    }

    /// Whether there is a critical fault requiring intervention.
    ///
    /// `FAULT2_IIN_OC` is **not** counted here: the tablet's vendor DT disables
    /// this protection (`ln8000_charger,iin-ocp-disable`,
    /// `nabu-sm8150.dtsi:239`), and `configure_protections` writes the same thing
    /// into `FAULT_CTRL`. The latch can survive from a previous session until POR,
    /// and the input must not be treated as absent because of it: a live measurement
    /// on 19.09 at 11:10 gave `FAULT2 = 0x80` at 8.256 V on the bus and
    /// `PostHvdcpMode = 3`. For reports the bit stays in [`Self::fault_summary`].
    #[must_use]
    pub const fn has_critical_fault(&self) -> bool {
        self.fault1_sts & (regs::FAULT1_WATCHDOG | regs::FAULT1_VBAT_OV | regs::FAULT1_VAC_OV) != 0
            || self.safety_sts & (regs::SAFETY_NTC_SHUTDOWN | regs::SAFETY_TEMP_MAX) != 0
    }

    /// Textual fault description for the journal (empty string if there are no faults).
    ///
    /// Hardware protections take priority: overtemperature and the battery leaving
    /// its range are more dangerous than an expired watchdog timer.
    #[must_use]
    pub const fn fault_summary(&self) -> &'static str {
        if self.safety_sts & regs::SAFETY_TEMP_MAX != 0 {
            "temp_max"
        } else if self.safety_sts & regs::SAFETY_NTC_SHUTDOWN != 0 {
            "ntc_shutdown"
        } else if self.fault1_sts & regs::FAULT1_VBAT_OV != 0 {
            "vbat_ov"
        } else if self.fault1_sts & regs::FAULT1_VAC_OV != 0 {
            "vac_ov"
        } else if self.fault2_sts & regs::FAULT2_IIN_OC != 0 {
            "iin_oc"
        } else if self.fault1_sts & regs::FAULT1_WATCHDOG != 0 {
            "watchdog"
        } else if self.fault1_sts & regs::FAULT1_VAC_UNPLUG != 0 {
            "vac_unplug"
        } else {
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_registers_follow_driver_mapping() {
        // Codes are 10-bit and are read as a pair from the channel register;
        // the channels overlap in bytes - that is a property of the hardware, not a bug.
        assert_eq!(AdcChannel::Iin.register(), regs::ADC_FIRST_STS);
        assert_eq!(AdcChannel::Vac.register(), regs::ADC_FIRST_STS + 1);
        assert_eq!(AdcChannel::Vin.register(), regs::ADC_FIRST_STS + 2);
        assert_eq!(AdcChannel::Vout.register(), regs::ADC_FIRST_STS + 3);
        // The ADC05 register (0x0D) is unused: VBAT and the rest go one further on.
        assert_eq!(AdcChannel::Vbat.register(), regs::ADC_FIRST_STS + 5);
        assert_eq!(AdcChannel::DieTemp.register(), regs::ADC_FIRST_STS + 6);
        assert_eq!(AdcChannel::TsBat.register(), regs::ADC_FIRST_STS + 7);
        assert_eq!(AdcChannel::TsBus.register(), regs::ADC_FIRST_STS + 8);
        assert_eq!(AdcChannel::Vbat.adc_index(), 6);
        assert_eq!(AdcChannel::TsBus.adc_index(), 9);
        assert_eq!(regs::ADC_LAST_STS, 0x12);
    }

    #[test]
    fn adc_decoding_matches_constants() {
        assert_eq!(AdcChannel::Vbat.decode(0x0000), 0);
        assert_eq!(AdcChannel::Vbat.decode(0x0064), 500_000);
        assert_eq!(AdcChannel::Vbat.decode(107 * 256 + 33), 4_005_000);
        // Live nabu raw pairs (sts[0]=ADC06, sts[1]=ADC07), Android VBAT packing.
        // Switching artifact: 0xBC/0x5B → code 956 → 4780 mV (≈ Vin/2).
        assert_eq!(AdcChannel::Vbat.decode(0x5B * 256 + 0xBC), 4_780_000);
        // Standby pack: 0x7E/0x7B → code 894 → 4470 mV (Nabu float).
        assert_eq!(AdcChannel::Vbat.decode(0x7B * 256 + 0x7E), 4_470_000);
        assert_eq!(AdcChannel::Iin.decode(0x0064), 489_000);
        assert_eq!(AdcChannel::Vin.decode(34 * 256 + 168), 8_864_000);
        assert_eq!(AdcChannel::Vac.decode(168 * 256 + 156), 8_896_000);
        // Reference formula: (935 - raw) * 4350 / 1000, clamped to [-250; 1600].
        assert_eq!(AdcChannel::DieTemp.decode(13 * 256 + 107), 334);
        assert_eq!(AdcChannel::DieTemp.decode(14 * 256 + 156), 0);
        assert_eq!(AdcChannel::DieTemp.decode(0), 1_600);
        assert_eq!(AdcChannel::DieTemp.decode(15 * 256 + 252), -250);
        assert_eq!(AdcChannel::TsBat.decode(0x0000), 0);
    }

    #[test]
    fn status_flags_are_interpreted() {
        let status = Status {
            sys_sts: regs::SYS_STS_SWITCHING_ENABLED | regs::SYS_STS_IIN_LOOP,
            op_mode: OpMode::Switching,
            safety_sts: 0,
            fault1_sts: 0,
            fault2_sts: 0,
            ldo_sts: regs::LDO_CHARGE_TERM,
        };
        assert!(status.iin_loop_active());
        assert!(!status.vfloat_loop_active());
        assert!(status.charge_terminated());
        assert!(!status.has_critical_fault());
        assert_eq!(status.fault_summary(), "");
    }

    #[test]
    fn critical_faults_are_detected() {
        let watchdog = Status {
            sys_sts: 0,
            op_mode: OpMode::Standby,
            safety_sts: 0,
            fault1_sts: regs::FAULT1_WATCHDOG,
            fault2_sts: 0,
            ldo_sts: 0,
        };
        assert!(watchdog.has_critical_fault());
        assert!(watchdog.watchdog_expired());
        assert_eq!(watchdog.fault_summary(), "watchdog");

        let overtemp = Status {
            safety_sts: regs::SAFETY_TEMP_MAX,
            ..watchdog
        };
        assert!(overtemp.has_critical_fault());
        assert_eq!(overtemp.fault_summary(), "temp_max");
    }

    #[test]
    fn unnamed_vfault_bits_fail_volt_qual_without_a_critical_fault() {
        // Live frame 18.09: `FAULT1=0x21` - the VFAULTS group, but not a single
        // named bit. `has_critical_fault` stays silent, the vendor's
        // `volt_qual` does not: exactly that gap is what telemetry must show.
        let vfaults = Status {
            sys_sts: 0x02,
            op_mode: OpMode::Standby,
            safety_sts: 0,
            fault1_sts: 0x21,
            fault2_sts: 0x3F,
            ldo_sts: 0,
        };
        assert!(!vfaults.has_critical_fault());
        assert!(!vfaults.volt_qual(false));
        assert!(!vfaults.volt_qual(true));

        // Clean first stage + bit 5 in the second: the input is invalid only when
        // charging is enabled - that is how the vendor has it (`.c:592-604`).
        let second_stage = Status {
            fault1_sts: 0,
            fault2_sts: regs::FAULT2_VOLT_FAULT,
            ..vfaults
        };
        assert!(second_stage.volt_qual(false));
        assert!(!second_stage.volt_qual(true));

        // Both stages clean - the input is valid in any charge state.
        let clean = Status {
            fault1_sts: 0,
            fault2_sts: 0,
            ..vfaults
        };
        assert!(clean.volt_qual(true));
    }
}
