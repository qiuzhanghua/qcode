//! 门禁 1 与门禁 2（I18N.md §8.1）。
//!
//! 放在 `cargo test` 里，所以 `make check` 自动跑到，不需要单独接线。
//!
//! - **门禁 1**：`locales/app.yml` 里每个 key 必须 `en` 与 `zh-CN` 成对出现。
//!   缺一条时 `rust-i18n` **不报错**，只把 key 原样吐给用户 —— 只能靠这条盯。
//! - **门禁 2**：禁止**新增**裸中文 UI 字面量。现存 554 条记在基线里，只允许减少。
//!   靠人记不住 554 条，靠这条能记住（ADR-0105 §6）。
//!
//! 门禁 3（双语 golden）需要跑真二进制，做成脚本：`scripts/check-i18n-golden.sh`。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 仓库根：集成测试的 `CARGO_MANIFEST_DIR` 是 `crates/qcode-i18n`。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("定位不到仓库根")
}

// ------------------------------------------------------------------ 门禁 1

/// 递归收集叶子 key，并顺手校验每个叶子正好带齐 [`qcode_i18n::SUPPORTED`]。
///
/// 判定「叶子」的方式：一个 mapping 的**键里出现了 locale 名**，它就该是叶子。
/// 这样写而不是按深度判断，是因为 key 的层级本来就随命名自由（`a.b` 与 `a.b.c`
/// 可以并存），只有 locale 名是结构性的。
fn collect_keys(
    node: &serde_json::Value,
    path: &str,
    keys: &mut BTreeSet<String>,
    problems: &mut Vec<String>,
) {
    let Some(map) = node.as_object() else {
        // 叶子是字符串：`app.name.en: qcode`。
        return;
    };

    let locales: Vec<&String> = map
        .keys()
        .filter(|key| qcode_i18n::SUPPORTED.contains(&key.as_str()))
        .collect();

    if locales.is_empty() {
        for (name, child) in map {
            // 版本号不是 key。
            if name == "_version" {
                continue;
            }
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            collect_keys(child, &child_path, keys, problems);
        }
        return;
    }

    // 是叶子：必须正好是受支持的 locale 集合，且每个值都是字符串。
    for locale in &qcode_i18n::SUPPORTED {
        match map.get(*locale) {
            Some(serde_json::Value::String(_)) => {}
            Some(_) => problems.push(format!("{path}.{locale} 不是字符串")),
            None => problems.push(format!("{path} 缺少 {locale} 译文")),
        }
    }
    for extra in map
        .keys()
        .filter(|key| !qcode_i18n::SUPPORTED.contains(&key.as_str()))
    {
        problems.push(format!(
            "{path} 里出现了陌生的语言键 `{extra}`（只支持 {:?}）",
            qcode_i18n::SUPPORTED
        ));
    }
    keys.insert(path.to_string());
}

/// `locales/` 下全部 yml 声明的 key 集合（跨文件去重前的全量）。
///
/// 门禁 1 与门禁 4 共用：一个验「声明得完整」，一个验「引用的都存在」。
fn declared_keys() -> (BTreeSet<String>, Vec<String>) {
    // 扫描 `locales/` 下**全部** yml：翻译按区域分文件（gui-*.yml），这样多人/
    // 多批同时干活时不会抢同一个文件，而完整性仍由这一条统一盯。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("读不到 locales/")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yml" | "yaml")
            )
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "locales/ 里一份译文都没有");

    let mut keys = BTreeSet::new();
    let mut problems = Vec::new();
    for file in &files {
        let raw = std::fs::read_to_string(file).expect("读不了译文文件");
        let parsed: serde_json::Value = serde_saphyr::from_str(&raw)
            .unwrap_or_else(|error| panic!("{} 不是合法 YAML：{error}", file.display()));
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let mut local = BTreeSet::new();
        collect_keys(&parsed, "", &mut local, &mut problems);
        for key in local {
            // key 是全局的：两个文件写同一个 key 时后写的会静默覆盖先写的，
            // 那是最难查的一类重复，所以这里直接报错。
            if !keys.insert(key.clone()) {
                problems.push(format!("{name} 里重复定义了 key `{key}`"));
            }
        }
    }

    (keys, problems)
}

#[test]
fn every_key_has_both_locales() {
    let (keys, problems) = declared_keys();
    assert!(
        problems.is_empty(),
        "locales/ 的 key 不完整：\n  - {}",
        problems.join("\n  - ")
    );
    assert!(!keys.is_empty(), "locales/ 里一条译文都没有");
}

#[test]
fn every_referenced_key_exists() {
    let (keys, _) = declared_keys();
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for (crate_name, path, source) in production_sources() {
        for key in referenced_keys(&source) {
            checked += 1;
            if !keys.contains(&key) {
                let file = path
                    .strip_prefix(workspace_root())
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                missing.push(format!("[{crate_name}] {file}: `{key}`"));
            }
        }
    }
    // 引用数为 0 说明扫描器本身坏了，比「key 不存在」更值得报警。
    assert!(checked > 0, "一条译文引用都没扫到，扫描器坏了");
    assert!(
        missing.is_empty(),
        "源码引用了 locales/ 里不存在的 key（界面上会原样显示 key 字符串）：\n  - {}",
        missing.join("\n  - ")
    );
}

/// 门禁 5：每个声明的 key 在**运行期**都取得到。
///
/// 门禁 4 是静态的（源码里写的 key 有没有声明），这一条是反方向：声明了的 key
/// 嵌进二进制之后还取不取得回。两者都要，因为 `rust_i18n::i18n!` 在宏展开时读
/// yml 并嵌入译文，cargo 看不见那次读取 —— 只改 yml 不重编时，二进制里是旧译文，
/// 界面显示 `<locale>.<key>` 这种回退串，而所有「测试运行时读文件」的检查照样通过。
/// `build.rs` 的 `rerun-if-changed` 治因，这一条兜果。
#[test]
fn every_key_resolves_at_runtime() {
    let (keys, _) = declared_keys();
    assert!(!keys.is_empty(), "一个 key 都没声明");
    let mut broken = Vec::new();
    for key in &keys {
        for locale in qcode_i18n::SUPPORTED {
            let text = qcode_i18n::_rust_i18n_translate(locale, key);
            // 取不到时 rust-i18n 返回 `<locale>.<key>`。
            if text == format!("{locale}.{key}") {
                broken.push(format!("{locale}: {key}"));
            }
            if text.trim().is_empty() {
                broken.push(format!("{locale}: {key} 是空译文"));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "这些 key 取不到译文（译文没嵌进二进制？见 build.rs）：\n  - {}",
        broken.join("\n  - ")
    );
}

// ------------------------------------------------------------------ 门禁 2

/// 一个 crate 里的一条中文串（crate 名, 转义后的字面量）。
type Entry = (String, String);

/// 跨平台排序稳定的基线行：`crate\t字面量`。
fn baseline_line(entry: &Entry) -> String {
    format!("{}\t{}", entry.0, entry.1)
}

fn read_baseline(path: &Path) -> BTreeSet<Entry> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "读不到基线 {}：{error}\n\
             （先跑一次 `QCODE_I18N_UPDATE_BASELINE=1 cargo test -p qcode-i18n` 生成）",
            path.display()
        )
    });
    text.lines()
        // 只吃掉 Windows 的 `\r`：字面量本身的尾随空格是内容的一部分，
        // `trim_end()` 会把它们连同内容一起抹掉，于是每次扫描都误报「新增」。
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (crate_name, literal) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("基线行格式不对（缺 tab）：{line}"));
            (crate_name.to_string(), literal.to_string())
        })
        .collect()
}

/// 判断一个字节位置之后的字符是不是汉字（含扩展 A 与兼容区）。
fn starts_han(text: &str, byte_index: usize) -> bool {
    text[byte_index..].chars().next().is_some_and(is_han)
}

fn is_han(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF     // 扩展 A
        | 0x4E00..=0x9FFF   // 基本区
        | 0xF900..=0xFAFF   // 兼容汉字
        | 0x3040..=0x30FF   // 平假名 / 片假名（也是「非英文 UI」）
        | 0xFF00..=0xFFEF   // 全角标点与字母（中文标点常在这里）
    )
}

/// 抽出一个 `.rs` 文件里的字符串字面量内容。
///
/// 手写词法而不是用 `syn`：本仓库的底线是「不为一次静态检查新增依赖」，而这条
/// 检查只需要分清「字符串里」与「不在字符串里」。要处理的是注释（含 `//` 里的
/// 引号）、转义引号、以及 `r#"…"#` 原始字符串 —— 这三样恰好是注释里写中文最常见
/// 的形态。
fn string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            // 行注释
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // 块注释（可嵌套：Rust 允许）
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            // 原始字符串：r"…" / r#"…"# / br#"…"#
            b'r' if is_raw_open(bytes, i) => {
                let (content, next) = take_raw(bytes, source, i);
                out.push(content);
                i = next;
            }
            // 普通字符串（含 b"…"）：**不跨行**，未闭合时当作普通文本继续
            b'"' => {
                let (content, next) = take_quoted(bytes, source, i);
                out.push(content);
                i = next;
            }
            _ => i += 1,
        }
    }
    out
}

/// `r`、`br` 之后紧跟 `"` 或若干个 `#` 再跟 `"`。
fn is_raw_open(bytes: &[u8], i: usize) -> bool {
    let mut j = i;
    if bytes.get(j) == Some(&b'b') {
        j += 1;
    }
    if bytes.get(j) != Some(&b'r') {
        return false;
    }
    j += 1;
    while bytes.get(j) == Some(&b'#') {
        j += 1;
    }
    bytes.get(j) == Some(&b'"')
}

/// 解析一个原始字符串，返回（内容，下一个位置）。
fn take_raw(bytes: &[u8], source: &str, start: usize) -> (String, usize) {
    let mut j = start;
    if bytes.get(j) == Some(&b'b') {
        j += 1;
    }
    j += 1; // 跳过 `r`
    let mut hashes = 0usize;
    while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    j += 1; // 跳过开头的 `"`
    let content_start = j;
    while j < bytes.len() {
        if bytes[j] == b'"' {
            let mut k = j + 1;
            let mut seen = 0usize;
            while seen < hashes && bytes.get(k) == Some(&b'#') {
                seen += 1;
                k += 1;
            }
            if seen == hashes {
                return (source[content_start..j].to_string(), k);
            }
        }
        j += 1;
    }
    (source[content_start..].to_string(), bytes.len())
}

/// 解析一个普通字符串字面量，处理 `\` 转义。
fn take_quoted(bytes: &[u8], source: &str, start: usize) -> (String, usize) {
    let mut j = start + 1; // 跳过开头的 `"`
    let content_start = j;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return (source[content_start..j].to_string(), j + 1),
            // 未转义的换行说明这个引号不是字符串开头（例如 `'` 里或别处），
            // 直接退出，避免把后续整段代码吞进一个「字符串」。
            b'\n' => return (source[content_start..j].to_string(), j),
            _ => j += 1,
        }
    }
    (source[content_start..].to_string(), bytes.len())
}

/// 文件里第一条 `#[cfg(test)]` 的位置；其后的内容属于测试代码，不计入基线。
fn test_module_start(source: &str) -> usize {
    ["#[cfg(test)]", "#[cfg(all(test"]
        .iter()
        .filter_map(|marker| source.find(marker))
        .min()
        .unwrap_or(source.len())
}

/// 一个文件里所有**被引用的**译文 key。
///
/// 只认三种取译文的姿势：`tr("…")`、`trp("…")`、`_rust_i18n_translate(locale, "…")`。
/// 前两种是我们自己的包装，第三种是 rust-i18n 生成的函数（`qcode-i18n` 自己的
/// 测试用它钉住某个 locale）。
fn referenced_keys(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // 注释里的 `tr("…")` 是文档举例，不算引用。
        if bytes[i] == b'/' && matches!(bytes.get(i + 1), Some(b'/' | b'*')) {
            i = skip_comment(bytes, i);
            continue;
        }
        // 标识符边界：`tr(` 得是完整的标识符，不能是 `startr(`。
        let starts_ident = i == 0 || !is_ident_byte(bytes[i - 1]);
        if starts_ident {
            for callee in [&b"trp("[..], &b"tr("[..], &b"_rust_i18n_translate("[..]] {
                if !bytes[i..].starts_with(callee) {
                    continue;
                }
                // `(` 之后第一个字符串字面量就是 key。中间要是先撞上 `;` `{` `(`
                // `)`，说明这次调用没带字面量（例如传了个变量）。
                let mut j = i + callee.len();
                while j < bytes.len() && !matches!(bytes[j], b'"' | b';' | b'{' | b'(' | b')') {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'"') {
                    let (text, next) = take_quoted(bytes, source, j);
                    // 只收 key 形状的（含 `.`），免得把 `tr("x")` 这类巧合当 key。
                    if text.contains('.') {
                        out.push(text);
                    }
                    i = next;
                }
                break;
            }
        }
        // 按字节前进：非 ASCII 字节落在多字节字符中间也没关系，这里只做字节比较，
        // 不做 `&str` 切片（那会要求字符边界）。
        i += 1;
    }
    out
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 从注释开头跳到注释结尾（行注释停在换行处，块注释可嵌套）。
fn skip_comment(bytes: &[u8], start: usize) -> usize {
    if bytes.get(start + 1) == Some(&b'/') {
        let mut i = start + 2;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        return i;
    }
    let mut depth = 1usize;
    let mut i = start + 2;
    while i < bytes.len() && depth > 0 {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// 遍历 `crates/` 下的生产代码，返回 `(crate 名, 文件路径, 生产部分源码)`。
///
/// 与 [`scan_crate`] 同一套取舍：跳过 `tests/`、`*.rs` 之外的、`tests.rs` 与
/// `#[cfg(test)]` 之后的部分。
fn production_sources() -> Vec<(String, PathBuf, String)> {
    fn walk(crate_name: &str, dir: &Path, out: &mut Vec<(String, PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "tests" | "target") || name.starts_with('.') {
                    continue;
                }
                walk(crate_name, &path, out);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "tests.rs" || !name.ends_with(".rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let production = source[..test_module_start(&source)].to_string();
            out.push((crate_name.to_string(), path, production));
        }
    }

    let mut out = Vec::new();
    let crates = workspace_root().join("crates");
    let Ok(entries) = std::fs::read_dir(&crates) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        walk(&name, &dir.join("src"), &mut out);
    }
    out
}

/// 遍历 `crates/` 下的生产代码，收集带汉字的字符串字面量。
fn scan_crate(crate_name: &str, dir: &Path, into: &mut BTreeSet<Entry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `tests/` 是集成测试；`target/` 与隐藏目录不在 crates/ 里，但防一手。
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "tests" | "target") || name.starts_with('.') {
                continue;
            }
            scan_crate(crate_name, &path, into);
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // `src/tests.rs` 这类是测试模块（没有 `#[cfg(test)]` 包着，靠文件名识别）。
        if name == "tests.rs" || !name.ends_with(".rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let production = &source[..test_module_start(&source)];
        for literal in string_literals(production) {
            if literal.chars().any(is_han) {
                into.insert((crate_name.to_string(), literal.escape_debug().to_string()));
            }
        }
    }
}

/// 收集整个 `crates/` 的现状。
fn scan_workspace(root: &Path) -> BTreeSet<Entry> {
    let mut found = BTreeSet::new();
    let crates = root.join("crates");
    let Ok(entries) = std::fs::read_dir(&crates) else {
        panic!("找不到 {}", crates.display());
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        scan_crate(&name, &dir.join("src"), &mut found);
    }
    found
}

#[test]
fn no_new_bare_chinese_literals() {
    // 这个门禁要遍历整个仓库，而 `cargo test` 会并发跑其它测试；只读文件，无副作用。
    let root = workspace_root();
    let baseline_path = root.join("scripts").join("i18n-baseline.txt");
    let found = scan_workspace(&root);

    if std::env::var_os("QCODE_I18N_UPDATE_BASELINE").is_some() {
        let mut text = String::from(
            "# 门禁 2 的基线：现存裸中文 UI 字面量（I18N.md §8.1）。\n\
             # 格式：crate<TAB>转义后的字面量。\n\
             # 只允许减少。每批翻译完成后重跑一次：\n\
             #   QCODE_I18N_UPDATE_BASELINE=1 cargo test -p qcode-i18n\n",
        );
        for entry in &found {
            text.push_str(&baseline_line(entry));
            text.push('\n');
        }
        std::fs::write(&baseline_path, text).expect("写不了基线文件");
        return;
    }

    let baseline = read_baseline(&baseline_path);

    let added: Vec<&Entry> = found.difference(&baseline).collect();
    let removed: Vec<&Entry> = baseline.difference(&found).collect();

    let mut problems = Vec::new();
    if !added.is_empty() {
        problems.push(format!(
            "新增了 {} 条裸中文字面量。UI 文案请走 `qcode_i18n::tr()`；\
             测试/提示信息请确认它确实不面向用户：",
            added.len()
        ));
        for (crate_name, literal) in added.iter().take(30) {
            problems.push(format!("  + [{crate_name}] {literal}"));
        }
        if added.len() > 30 {
            problems.push(format!("  …还有 {} 条", added.len() - 30));
        }
    }
    if !removed.is_empty() {
        problems.push(format!(
            "基线里有 {} 条已经不存在了 —— 棘轮该往前走一格。",
            removed.len()
        ));
        for (crate_name, literal) in removed.iter().take(10) {
            problems.push(format!("  - [{crate_name}] {literal}"));
        }
        if removed.len() > 10 {
            problems.push(format!("  …还有 {} 条", removed.len() - 10));
        }
        problems.push(
            "确认无误后执行：\n    QCODE_I18N_UPDATE_BASELINE=1 cargo test -p qcode-i18n"
                .to_string(),
        );
    }

    assert!(
        problems.is_empty(),
        "门禁 2（禁止新增裸中文）失败：\n{}",
        problems.join("\n")
    );
}

// ------------------------------------------------------------ 词法器自测

#[test]
fn lexer_sees_strings_not_comments() {
    let src = r####"
// 注释里的"中文"不算
/* 块注释 "也不算" */
fn main() {
    let a = "真的算」";
    let b = "转义 \" 引号";
    let c = r#"原始 "中文" 字符串"#;
    let d = 'x';
    let e = "ASCII only";
}
"####;
    let literals = string_literals(src);
    let with_han: Vec<&String> = literals.iter().filter(|s| s.chars().any(is_han)).collect();
    assert_eq!(
        with_han.len(),
        3,
        "应恰好认出 3 条含汉字的字面量，实际：{literals:?}"
    );
    assert!(literals.iter().any(|s| s.contains("原始")));
}

#[test]
fn test_module_is_excluded() {
    let src = "fn f() {}\n#[cfg(test)]\nmod tests { const X: &str = \"测试里的中文\"; }";
    let production = &src[..test_module_start(src)];
    assert!(
        !string_literals(production)
            .iter()
            .any(|s| s.contains("测试"))
    );
}

#[test]
fn han_detection_covers_fullwidth_punctuation() {
    assert!(is_han('中'));
    assert!(is_han('，'));
    assert!(is_han('：'));
    assert!(!is_han('A'));
    assert!(!starts_han("abc", 0));
}
