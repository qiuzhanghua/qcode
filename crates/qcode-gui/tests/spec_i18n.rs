//! 门禁 8：设置页数据表的 key 可达性。
//!
//! `forms.rs` 的 `FIELDS` / `TABLES` 是数据表，键由**路径**拼出来：
//! `gui.setup.spec.<field path>.label` / `.help`、`gui.setup.spec.<table path>.title` /
//! `.col.<列key>`。这类 key 是运行期拼的，`qcode-i18n` 的静态扫描（门禁 4/6）看不见，
//! 所以门禁 6 显式放行 `gui.setup.spec.` 前缀，由这一条接管——和 `cli.cmd.*` 与
//! 门禁 7 的关系一样。
//!
//! 三个方向都要查：
//!
//! 1. **表里的 key 有译文**（两种语言都有）——否则中文界面里那一格静默留英文；
//! 2. **声明的 key 有主**——拼错或删了字段没删译文，就是死重量；
//! 3. **同一张表里 key 不重复**——重复时后一条静默覆盖前一条，两行显示同一个名字。

use qcode_gui::forms::{FIELDS, GUI_FIELDS, TABLES};
use std::collections::BTreeSet;

fn qcode_i18n_locales() -> [&'static str; 2] {
    ["en", "zh-CN"]
}

/// 数据表里**实际被查**的 key。
///
/// 一定要直接读字段里的字符串（`field.label` / `field.help` / `table.title` /
/// `column.label`），**不要**用 `path` 重新拼一遍。第一版就是这么写的，于是它只
/// 验证了「yml 里每个 path 都有条目」，而代码里 key 拼错（`maxTurn`）它一个字都
/// 说不出——反向验证时才暴露：故意把 key 改错，门禁照样全绿。
///
/// 现在这条读的是**真正会被 `tr()` 拿到的那个字符串**，所以两个方向都实：
/// 代码里的 key 拼错 → 查不到（第一条测试红）；yml 里的孤儿 → 没主（第二条红）。
fn keys_in_tables() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // `GUI_FIELDS`（外观页）与 `FIELDS` 是两张表，一张都不能漏——第一版只走了
    // `FIELDS`，于是外观页那 5 个字段的译文被这条门禁判成「声明了却没人用」。
    for field in FIELDS.iter().chain(GUI_FIELDS.iter()) {
        out.insert(field.label.to_string());
        out.insert(field.help.to_string());
    }
    for table in TABLES {
        out.insert(table.title.to_string());
        for column in table.columns {
            out.insert(column.label.to_string());
        }
    }
    out
}

/// `app.yml` 里声明的 `gui.setup.spec.*`。
fn declared_spec_keys() -> BTreeSet<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../qcode-i18n/locales");
    let mut keys = BTreeSet::new();
    let entries = std::fs::read_dir(&dir).expect("读不到 locales/");
    for entry in entries.flatten() {
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("读不了译文文件");
        let mut stack: Vec<String> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim_end();
            if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
                continue;
            }
            let indent = trimmed.len() - trimmed.trim_start().len();
            let Some((key, value)) = trimmed.trim_start().split_once(':') else {
                continue;
            };
            stack.truncate(indent / 2);
            if matches!(key, "en" | "zh-CN") {
                let joined = stack.join(".");
                if joined.starts_with("gui.setup.spec.") {
                    keys.insert(joined);
                }
                continue;
            }
            if value.trim().is_empty() {
                stack.push(key.to_string());
            }
        }
    }
    keys
}

#[test]
fn every_spec_key_resolves_in_both_locales() {
    let missing: Vec<String> = keys_in_tables()
        .into_iter()
        .filter(|key| {
            qcode_i18n_locales()
                .iter()
                .any(|locale| qcode_i18n::try_tr_in(locale, key).is_none())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "设置页数据表里有 {} 个 key 在某种语言下取不到（界面会显示 key 本身）：\n  - {}",
        missing.len(),
        missing.join("\n  - ")
    );
}

#[test]
fn every_declared_spec_key_is_used() {
    let used = keys_in_tables();
    let declared = declared_spec_keys();
    assert!(!declared.is_empty(), "一条 gui.setup.spec.* 都没读到");

    let orphans: Vec<&String> = declared.difference(&used).collect();
    assert!(
        orphans.is_empty(),
        "这些 gui.setup.spec.* 声明了但数据表里没有对应的字段/列（key 拼错，或字段已删）：\n  - {}",
        orphans
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>()
            .join("\n  - ")
    );
}

/// 同一张表里两列 key 重复时，`ColumnSpec` 的查找会静默取到同一个格子，
/// 两行显示同一个名字——这种错没有别的门禁能发现。
#[test]
fn spec_paths_are_unique() {
    let mut seen = BTreeSet::new();
    let mut duplicates = Vec::new();
    for field in FIELDS.iter().chain(GUI_FIELDS.iter()) {
        if !seen.insert(field.path.join(".")) {
            duplicates.push(format!("字段 {}", field.path.join(".")));
        }
    }
    for table in TABLES {
        let path = table.path.join(".");
        if !seen.insert(path.clone()) {
            duplicates.push(format!("表 {path}（与某个字段或另一张表同路径）"));
        }
        let mut columns = BTreeSet::new();
        for column in table.columns {
            if !columns.insert(column.key) {
                duplicates.push(format!("表 {path} 的列 {}", column.key));
            }
        }
    }
    assert!(
        duplicates.is_empty(),
        "设置页数据表里有重复路径（译文与查找都会互相覆盖）：\n  - {}",
        duplicates.join("\n  - ")
    );
}

/// 每个字段都必须挂在某个页面上，否则它在设置页里根本不出现（写好了却没人看得见）。
#[test]
fn every_field_belongs_to_a_page() {
    use qcode_gui::forms::Page;
    for field in FIELDS.iter().chain(GUI_FIELDS.iter()) {
        assert!(
            Page::ALL.contains(&field.page),
            "字段 {} 的 page 不在 Page::ALL 里",
            field.path.join(".")
        );
    }
}
