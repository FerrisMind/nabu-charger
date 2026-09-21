//! Cross-check of the operator documentation against the INF defaults.
//!
//! A separate revision pitfall: a threshold edit in `ln8000_kmdf.inx` is forgotten
//! and not carried into `docs/DEPLOY-LN8000.md`, so someone restoring the profile
//! from the document rolls back the corrected value (as happened with 4.42 V: the
//! document promised the QC3 loop limit as the charge target). The test reads both
//! files and fails as soon as the INF defaults and the document table drift apart.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::missing_panics_doc
)]

use std::fs;
use std::path::PathBuf;

/// Repository root relative to this crate's directory (`crates/ln8000`).
fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(relative);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Parses `HKR, Parameters, <Name>, %REG_DWORD%, <value>` lines from the INF.
fn inf_defaults(inf: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in inf.lines() {
        let Some(rest) = line.trim().strip_prefix("HKR, Parameters,") else {
            continue;
        };
        let fields: Vec<&str> = rest.split(',').map(str::trim).collect();
        if fields.len() < 3 {
            continue;
        }
        let (name, value) = (fields[0], fields[2]);
        if name.is_empty() || value.is_empty() {
            continue;
        }
        found.push((name.to_owned(), value.to_owned()));
    }
    found
}

#[test]
fn docs_profile_table_matches_inf_defaults() {
    let inf = repo_file("crates/ln8000-kmdf/ln8000_kmdf.inx");
    let doc = repo_file("docs/DEPLOY-LN8000.md");

    let defaults = inf_defaults(&inf);
    assert!(
        defaults.len() >= 15,
        "the INF must keep at least fifteen parameters, found {}",
        defaults.len()
    );

    for (name, value) in &defaults {
        let row = doc
            .lines()
            .find(|line| line.contains(&format!("`{name}`")))
            .unwrap_or_else(|| panic!("docs/DEPLOY-LN8000.md has no parameter {name}"));
        assert!(
            row.contains(&format!("| {value} |")),
            "document row for {name} does not match the INF: expected {value}, row: {row}"
        );
    }

    // Separate safeguard against charge thresholds drifting apart: 4.42 V is the
    // QC3 loop limit, and it must not stand in the document as the charge target.
    for line in doc.lines() {
        if line.contains("`VbatFloatUv`") || line.contains("`VbatReduceUv`") {
            assert!(
                !line.contains("4420000"),
                "4.42 V is again presented in the document as the charge target: {line}"
            );
        }
    }
}

#[test]
fn docs_guard_table_matches_standard_limits() {
    let doc = repo_file("docs/DEPLOY-LN8000.md");
    let limits = ln8000::GuardLimits::standard();

    // Thresholds of the `ln8000::guard` module: the document writes them with a
    // space as the digit separator, so we check both "as in code" and "as in the table".
    let checks = [
        ("temp_reduce_dc", limits.temp_reduce_dc.to_string()),
        ("temp_bypass_dc", limits.temp_bypass_dc.to_string()),
        ("temp_stop_dc", limits.temp_stop_dc.to_string()),
        ("iin_max_ua", group_digits(limits.iin_max_ua)),
        ("vbat_reduce_uv", group_digits(limits.vbat_reduce_uv)),
    ];
    for (name, value) in checks {
        let row = doc
            .lines()
            .find(|line| line.contains(&format!("`{name}`")))
            .unwrap_or_else(|| panic!("docs/DEPLOY-LN8000.md has no threshold {name}"));
        assert!(
            row.contains(&value),
            "threshold {name} in the document drifts from the code: expected {value}, row: {row}"
        );
    }

    // Restore hysteresis (F9) is described in the document with the same numbers as the code.
    assert!(
        doc.contains("50 mV") || doc.contains("50000"),
        "the document has no voltage restore hysteresis"
    );
}

#[test]
fn guard_band_row_names_the_actual_profile() {
    // Revision pitfall: the fold-back band row promised "2 A at the 2.8 A profile",
    // although the parameter table above sets `IinLimitUa = 2000000`. With the INF
    // defaults the profile and the target are equal, the band coincides with the
    // profile, and a reader of the document concluded that the step-down would fire
    // by itself. We check that the row names both profiles: INF default and code.
    let doc = repo_file("docs/DEPLOY-LN8000.md");
    let inf = repo_file("crates/ln8000-kmdf/ln8000_kmdf.inx");
    let defaults = inf_defaults(&inf);
    let inf_profile: u32 = defaults
        .iter()
        .find(|(name, _)| name == "IinLimitUa")
        .unwrap_or_else(|| panic!("the INF has no IinLimitUa parameter"))
        .1
        .parse()
        .unwrap_or_else(|err| panic!("IinLimitUa in the INF is not a number: {err}"));

    let row = doc
        .lines()
        .find(|line| line.contains("`temp_reduce_dc`"))
        .unwrap_or_else(|| panic!("docs/DEPLOY-LN8000.md has no temp_reduce_dc threshold"));
    assert!(
        row.contains(&group_digits(inf_profile)),
        "the fold-back band row does not name the INF profile ({}): {row}",
        group_digits(inf_profile)
    );
    assert!(
        row.contains(&group_digits(
            ln8000::PumpConfig::for_qc35_class_b().iin_limit_ua
        )),
        "the fold-back band row does not name the code QC profile: {row}"
    );
    assert!(
        !row.contains("2 A at the 2.8 A profile"),
        "the promise \"2 A at the 2.8 A profile\" contradicts the parameter table: {row}"
    );
}

/// Inserts a space between digit groups: `3_500_000` becomes "3 500 000".
fn group_digits(value: u32) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}
