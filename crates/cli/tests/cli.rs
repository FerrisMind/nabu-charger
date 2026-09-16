//! Тесты командной строки: реальный запуск бинарника `nabu-charger`.

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
    let dir = tempfile::tempdir().expect("временный каталог");
    let journal = dir.path().join("demo.jsonl");
    let output = Command::new(binary())
        .arg("--log")
        .arg("warn")
        .arg("--journal")
        .arg(&journal)
        .arg("demo")
        .output()
        .expect("бинарник запускается");

    assert!(
        output.status.success(),
        "демо должно завершиться успешно: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("HVDCP3P5"),
        "в выводе есть родной блок: {stdout}"
    );
    assert!(stdout.contains("3000000"), "в выводе есть лимит тока");
    assert!(journal.exists(), "журнал создан");

    let text = std::fs::read_to_string(&journal).expect("журнал читается");
    assert!(text.lines().count() > 10, "журнал содержит записи");
}

#[test]
fn verify_passes_self_check() {
    let output = Command::new(binary())
        .arg("--log")
        .arg("error")
        .arg("verify")
        .output()
        .expect("бинарник запускается");
    assert!(
        output.status.success(),
        "самопроверка должна проходить: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("самопроверка: пройдена"), "вывод: {stdout}");
    assert!(
        stdout.contains("APSD_RESULT_STATUS"),
        "паспорт содержит карту регистров"
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
        .expect("бинарник запускается");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("HVDCP2"), "адаптер распознан: {stdout}");
    assert!(stdout.contains("1500000"), "ток для QC2: {stdout}");
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
        .expect("бинарник запускается");
    assert!(
        !output.status.success(),
        "подключение к закрытому порту даёт отказ"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ошибка:"), "понятное сообщение: {stderr}");
}
