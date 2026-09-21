//! Driver specification: version, hardware, register map, policy table.
//!
//! Used by the `verify` command and included in the acceptance report, so that the
//! specification is checkable rather than retold.

use crate::apsd::AdapterType;
use crate::policy::{self, Qc35Support};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Device model.
pub const MODEL: &str = "Xiaomi Pad 5 (nabu)";

/// Platform.
pub const PLATFORM: &str = "Qualcomm SM8150 (Snapdragon 860), Windows on ARM64";

/// Charger in the PMIC.
pub const CHARGER_BLOCK: &str = "PM8150B SMB (USBIN peripheral, base 0x1300)";

/// External charge pump.
pub const CHARGE_PUMP: &str = "LN8000, I2C 0x51, ACPI node PEIC (QCOM057E)";

/// One register from the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterSpec {
    /// Address.
    pub addr: u16,
    /// Name from the documentation.
    pub name: &'static str,
    /// Purpose.
    pub purpose: &'static str,
}

/// Registers the driver works with.
pub const REGISTERS: &[RegisterSpec] = &[
    RegisterSpec {
        addr: crate::regs::APSD_STATUS,
        name: "APSD_STATUS",
        purpose: "detection state machine status: readiness and the Quick Charge flag",
    },
    RegisterSpec {
        addr: crate::regs::APSD_RESULT_STATUS,
        name: "APSD_RESULT_STATUS",
        purpose: "detection result: adapter type in bits 6:0",
    },
    RegisterSpec {
        addr: crate::regs::QC_CHANGE_STATUS,
        name: "QC_CHANGE_STATUS",
        purpose: "Quick Charge negotiation state",
    },
    RegisterSpec {
        addr: crate::regs::CMD_APSD,
        name: "CMD_APSD",
        purpose: "detection rerun (APSD_RERUN bit)",
    },
    RegisterSpec {
        addr: crate::regs::CMD_ICL_OVERRIDE,
        name: "CMD_ICL_OVERRIDE",
        purpose: "enable the forced input current limit",
    },
    RegisterSpec {
        addr: crate::regs::USBIN_CURRENT_LIMIT_CFG,
        name: "USBIN_CURRENT_LIMIT_CFG",
        purpose: "input current limit code (100 mA grid)",
    },
    RegisterSpec {
        addr: crate::regs::USBIN_ICL_OPTIONS,
        name: "USBIN_ICL_OPTIONS",
        purpose: "extra current limit options",
    },
    RegisterSpec {
        addr: crate::regs::HVDCP_PULSE_COUNT_MAX,
        name: "HVDCP_PULSE_COUNT_MAX",
        purpose: "Quick Charge 2.0 voltage selection (bits 7:6)",
    },
];

/// Policy table row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterSpec {
    /// Adapter type.
    pub adapter: AdapterType,
    /// Pattern in `APSD_RESULT_STATUS`.
    pub pattern: u8,
    /// Input current limit from the policy.
    pub icl_ua: u32,
    /// Whether the charge pump is eligible.
    pub pump_eligible: bool,
}

/// Default policy table (QC3.5 without authentication).
#[must_use]
pub fn adapter_table() -> [AdapterSpec; 7] {
    let qc35 = Qc35Support::default();
    [
        AdapterType::Sdp,
        AdapterType::Cdp,
        AdapterType::Dcp,
        AdapterType::Float,
        AdapterType::Hvdcp2,
        AdapterType::Hvdcp3,
        AdapterType::Hvdcp3P5,
    ]
    .map(|adapter| {
        let policy = policy::policy_for(adapter, qc35);
        AdapterSpec {
            adapter,
            pattern: adapter.apsd_pattern(),
            icl_ua: policy.icl_ua,
            pump_eligible: policy.pump_eligible,
        }
    })
}

/// Textual driver specification.
#[cfg(feature = "std")]
#[must_use]
pub fn render_spec() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "driver      : core {VERSION}");
    let _ = writeln!(out, "device      : {MODEL}");
    let _ = writeln!(out, "platform    : {PLATFORM}");
    let _ = writeln!(out, "charger     : {CHARGER_BLOCK}");
    let _ = writeln!(out, "charge pump : {CHARGE_PUMP}");
    let _ = writeln!(out, "\nregisters:");
    for spec in REGISTERS {
        let _ = writeln!(
            out,
            "  0x{:04X}  {:<26} {}",
            spec.addr, spec.name, spec.purpose
        );
    }
    let _ = writeln!(out, "\ncurrent policy by adapter type:");
    for spec in adapter_table() {
        let _ = writeln!(
            out,
            "  {:<9} pattern 0x{:02X}  {:>8} µA  pump: {}",
            spec.adapter.label(),
            spec.pattern,
            spec.icl_ua,
            if spec.pump_eligible { "yes" } else { "no" }
        );
    }
    out
}
