//! Сверка операторской документации с умолчаниями INF.
//!
//! Отдельная ловушка ревизий: правку порога в `ln8000_kmdf.inx` забывают
//! перенести в `docs/DEPLOY-LN8000.md`, и человек, восстанавливающий профиль по
//! документу, откатывает исправленное значение (так было с 4,42 В: документ
//! обещал лимит петли QC3 как цель заряда). Тест читает оба файла и падает,
//! как только умолчания INF и таблица профилей в документе разъезжаются.
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

/// Корень репозитория от каталога этого крейта (`crates/ln8000`).
fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(relative);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("не читается {}: {err}", path.display()))
}

/// Разбирает строки `HKR, Parameters, <Имя>, %REG_DWORD%, <значение>` из INF.
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
        "в INF должно остаться не меньше пятнадцати параметров, найдено {}",
        defaults.len()
    );

    for (name, value) in &defaults {
        let row = doc
            .lines()
            .find(|line| line.contains(&format!("`{name}`")))
            .unwrap_or_else(|| panic!("в docs/DEPLOY-LN8000.md нет параметра {name}"));
        assert!(
            row.contains(&format!("| {value} |")),
            "строка документа для {name} не совпадает с INF: ожидалось значение {value}, строка: {row}"
        );
    }

    // Отдельная страховка от разъехавшихся порогов заряда: 4,42 В — лимит петли
    // QC3, и он не должен стоять в документе на месте цели заряда.
    for line in doc.lines() {
        if line.contains("`VbatFloatUv`") || line.contains("`VbatReduceUv`") {
            assert!(
                !line.contains("4420000"),
                "4,42 В в документе снова подано как цель заряда: {line}"
            );
        }
    }
}

#[test]
fn docs_guard_table_matches_standard_limits() {
    let doc = repo_file("docs/DEPLOY-LN8000.md");
    let limits = ln8000::GuardLimits::standard();

    // Пороги модуля `ln8000::guard`: в документе они записаны с пробелом как
    // разделителем разрядов, поэтому сверяем и «как в коде», и «как в таблице».
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
            .unwrap_or_else(|| panic!("в docs/DEPLOY-LN8000.md нет порога {name}"));
        assert!(
            row.contains(&value),
            "порог {name} в документе разошёлся с кодом: ожидалось {value}, строка: {row}"
        );
    }

    // Гистерезисы возврата (F9) описаны в документе теми же числами, что в коде.
    assert!(
        doc.contains("50 мВ") || doc.contains("50000"),
        "в документе нет гистерезиса возврата по напряжению"
    );
}

#[test]
fn guard_band_row_names_the_actual_profile() {
    // Ловушка ревизии: строка полосы среза обещала «2 А при профиле 2,8 А»,
    // хотя таблица параметров выше задаёт `IinLimitUa = 2000000`. При
    // умолчаниях INF профиль и цель равны, полоса совпадает с профилем, и
    // читатель документа делал вывод, что ступень снижения сработает сама.
    // Проверяем, что в строке названы оба профиля: умолчание INF и кодовый.
    let doc = repo_file("docs/DEPLOY-LN8000.md");
    let inf = repo_file("crates/ln8000-kmdf/ln8000_kmdf.inx");
    let defaults = inf_defaults(&inf);
    let inf_profile: u32 = defaults
        .iter()
        .find(|(name, _)| name == "IinLimitUa")
        .unwrap_or_else(|| panic!("в INF нет параметра IinLimitUa"))
        .1
        .parse()
        .unwrap_or_else(|err| panic!("IinLimitUa в INF не число: {err}"));

    let row = doc
        .lines()
        .find(|line| line.contains("`temp_reduce_dc`"))
        .unwrap_or_else(|| panic!("в docs/DEPLOY-LN8000.md нет порога temp_reduce_dc"));
    assert!(
        row.contains(&group_digits(inf_profile)),
        "строка полосы среза не называет профиль INF ({}): {row}",
        group_digits(inf_profile)
    );
    assert!(
        row.contains(&group_digits(
            ln8000::PumpConfig::for_qc35_class_b().iin_limit_ua
        )),
        "строка полосы среза не называет кодовый профиль QC: {row}"
    );
    assert!(
        !row.contains("2 А при профиле 2,8 А"),
        "обещание «2 А при профиле 2,8 А» противоречит таблице параметров: {row}"
    );
}

/// Вставляет пробел между разрядами: `3_500_000` → «3 500 000».
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
