//! Command-line tests: a real launch of the `nabu-charger` binary.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nabu-charger"))
}

#[test]
fn demo_runs_and_writes_journal() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let journal = dir.path().join("demo.jsonl");
    let output = Command::new(binary())
        .arg("--log")
        .arg("warn")
        .arg("--journal")
        .arg(&journal)
        .arg("demo")
        .output()
        .expect("the binary starts");

    assert!(
        output.status.success(),
        "the demo must complete successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("HVDCP3P5"),
        "the output contains the stock charger: {stdout}"
    );
    assert!(
        stdout.contains("3000000"),
        "the output contains the current limit"
    );
    assert!(journal.exists(), "the journal was created");

    let text = std::fs::read_to_string(&journal).expect("the journal is readable");
    assert!(text.lines().count() > 10, "the journal contains records");
}

#[test]
fn verify_passes_self_check() {
    let output = Command::new(binary())
        .arg("--log")
        .arg("error")
        .arg("verify")
        .output()
        .expect("the binary starts");
    assert!(
        output.status.success(),
        "the self-check must pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("self-check: passed"), "output: {stdout}");
    assert!(
        stdout.contains("APSD_RESULT_STATUS"),
        "the spec contains the register map"
    );
}

#[test]
fn detect_reports_expected_current_for_mock_adapter() {
    let output = Command::new(binary())
        .arg("--log")
        .arg("error")
        .arg("detect")
        .arg("--adapter")
        .arg("hvdcp2")
        .output()
        .expect("the binary starts");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("HVDCP2"), "adapter detected: {stdout}");
    assert!(stdout.contains("1500000"), "current for QC2: {stdout}");
}

#[test]
fn detect_fails_with_clear_error_on_dead_socket() {
    let output = Command::new(binary())
        .arg("--log")
        .arg("error")
        .arg("detect")
        .arg("--transport")
        .arg("tcp")
        .arg("--addr")
        .arg("127.0.0.1:1")
        .output()
        .expect("the binary starts");
    assert!(
        !output.status.success(),
        "connecting to a closed port fails"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "clear message: {stderr}");
}
