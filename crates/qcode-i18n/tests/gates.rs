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

/// 门禁 6：声明的 key 必须有人引用（没有孤儿）。
///
/// 门禁 4 查的是「引用了不存在的 key」，反过来没人查：一条翻好的 key 如果随着
/// 代码重构失去调用点，它会安静地留在表里——下次有人改文案时改它、改完发现界面
/// 没变，或者更糟：两条 key 表达同一件事，只有一条还在用。孤儿是翻译表的腐坏起点。
///
/// **动态 key 例外**：本仓库所有取译文的地方都用字面量（`tr("a.b")`），包括
/// `match` 分支里的那些（`weekday_name` 之类）。将来若出现拼出来的 key，这条
/// 门禁会误报，那时的正解是在调用处加白名单注释，而不是关掉这条门禁。
#[test]
fn every_declared_key_is_referenced() {
    let (keys, _) = declared_keys();
    // **扫全文，含测试代码**：`app.name` / `app.error_prefix` 只被本 crate 的
    // 自检测试引用（P0 用来证明链路通的那两条），它们是合法在用的。孤儿要抓的是
    // 「谁都不提它了」，不是「只有测试提它」。
    let mut referenced = std::collections::BTreeSet::new();
    for (_, _, source) in all_sources() {
        referenced.extend(referenced_keys(&source));
    }
    // `cli.cmd.*` 是**动态寻址**的：`clap_i18n::localize` 按命令树路径拼 key，
    // 静态扫描看不见。它的两个方向由另一道门禁盯着
    // （`crates/qcode/src/cli_i18n_gate.rs`：遍历真实的 `Command` 树，既查「声明
    // 了却查不到」也查「查得到却没译文」）。这里放行，并**只**放行这一个前缀。
    let orphans: Vec<&String> = keys
        .difference(&referenced)
        .filter(|key| !key.starts_with("cli.cmd."))
        // 设置页数据表同理（`forms.rs` 的 `FIELDS` / `TABLES` 也是按路径拼 key）。
        // 它的双向覆盖由 `crates/qcode-gui/tests/spec_i18n.rs` 盯着。
        .filter(|key| !key.starts_with("gui.setup.spec."))
        .collect();
    assert!(
        orphans.is_empty(),
        "这些 key 声明了但没人引用（翻译表在腐坏）：\n  - {}",
        orphans
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>()
            .join("\n  - ")
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
            // 长的在前：先试 `trp_in(`，否则 `trp(` 会把它截住。
            for callee in [
                &b"trp_in("[..],
                &b"trp("[..],
                &b"tr("[..],
                &b"_rust_i18n_translate("[..],
            ] {
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

/// 遍历 `crates/` 下的**全部** `.rs`（含测试代码），返回 `(crate 名, 路径, 全文)`。
fn all_sources() -> Vec<(String, PathBuf, String)> {
    let mut out = Vec::new();
    fn walk(crate_name: &str, dir: &Path, out: &mut Vec<(String, PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                walk(crate_name, &path, out);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            out.push((crate_name.to_string(), path, source));
        }
    }
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
        walk(&name, &dir, &mut out);
    }
    out
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

// ------------------------------------------------------------- 门禁 2 的英文面

/// 在源码里出现的、**处于 UI 文案位置**的英文字面量。
///
/// ## 为什么按位置判、不按内容判
///
/// 「像不像界面文案」没有可靠的语法判据：`"running"` / `"queue"` 是状态名，
/// `"ModelMenu"` 是 widget id，`"Terminal"` 是 key context，而 `"Sandbox"` 是
/// 界面标题——都是单个英文单词。靠大小写、空格去猜只会两头误判。
///
/// 所以这里只认**位置**：一个字符串字面量紧跟在下列「只可能装界面文案」的位置之后，
/// 才算 UI 文案。这些位置在 gui 里就那么多，位置集合比内容启发式稳得多。
///
/// 已知盲区（有意接受）：`format!("…")` 里拼出来的文案、`.child(StyledText…)` 的
/// 间接来源、以及没被列出的新 API。门禁是棘轮，位置集合可以长；宁可漏，不要误报到
/// 让人关掉它。
fn ui_text_positions() -> &'static [&'static str] {
    &[
        // 组件 API 里只收文案的参数。
        ".label(",
        ".aria_label(",
        // setup 页的分组标题。
        "group_heading(",
        // `FieldSpec` / `ColumnSpec` / `TableSpec` 的数据字段（`forms.rs`）。
        "label: ",
        "help: ",
        "title: ",
        // 工具结果摘要（ADR-0105 §5）：用户在工具卡片上直接读到的那句话。
        // `ToolOutput::ok(变量)` / `ok(tr(…))` 不以引号开头，自动不算。
        "ToolOutput::ok(",
        "ToolOutput::err(",
        "ToolOutput::cancelled(",
        // CLI 命令输出（`qc <cmd>` 跑完用户读的那几行，I18N.md §10.15）。
        // 只收 `println!` / `eprintln!` —— 它们是**直接输出**，没有误伤风险。
        "println!(",
        "eprintln!(",
        // `qc doctor` 的两参数检查行（I18N.md §10.17）。第一个参数是标签 key，
        // **第二个**才是明细——`is_multi_literal_marker` 会把两者都取出来，
        // 标签被 `is_key` 滤掉、明细照查。不收这几个标记时，doctor 的 22 个标签
        // 与约 45 条明细整批落在两条门禁之间：中文门禁只找汉字，英文门禁看这里。
        "self.ok(",
        "self.warn(",
        "self.fail(",
        "doctor.ok(",
        "doctor.warn(",
        "doctor.fail(",
        // **拼出来**的文案（I18N.md §10.18）。P3 之后这一片一直没被覆盖，理由
        // 当时是「`write!` / `push_str` 是通用拼装，`qcode-tools/macos.rs` 用它
        // 拼 sandbox-exec 的 profile DSL」。那个顾虑是真的，但代价是：TUI 的 16
        // 条斜杠命令提示、GUI 的 12 条 notice、工具结果的 6 条 —— 全都用
        // `push_notice` / `push_str` 拼，于是整批落在两条门禁之间。
        //
        // 现在收进来，用**文件级豁免**挡住真会坏的那一处（见
        // `intentional_english` 里的 `macos.rs`），而不是把整族写法排除掉。
        // `write!` 的纯占位符（`"{}\n"`）由 `is_placeholder_only` 过滤。
        "push_notice(",
        "write!(",
        "writeln!(",
        // `.push_str(` 是**方法调用**，不是宏——Rust 没有 `push_str!`。
        // 之前这里是 `push_str!(`：一个永远匹配不到任何东西的死标记，看着像
        // 覆盖、其实一条都没扫，`actions.rs` 的 "Follow the skill instructions
        // above now." 就这么留了一轮（I18N.md §10.19）。
        ".push_str(",
        // **走返回值**的错误串（I18N.md §10.19）。它们不经过任何 `println!`：
        // `main.rs` 统一 `eprintln!("error: {err}")`，所以每一个 `Err(…)` 最终
        // 都会原样打到用户眼前（`qc sessions delete nope` → `error: no such
        // session: nope`）。位置表按「输出调用」判，而这批是**构造错误值**，
        // 于是整批落在门外。
        //
        // 三条覆盖 133 处：`Err(format!(…))`、`Err("…".into())`、
        // `.ok_or_else(|| format!(…))`。`Err(` 不误伤 `map_err(`（大小写不同）
        // 也不误伤 `Err(e) =>` 匹配臂（括号里没有字面量）。
        "Err(",
        ".ok_or_else(",
        ".ok_or(",
        // `.map_err(|_| format!("…"))`：把下层错误换成自己的一句。整条错误串
        // 只在这里出现一次，不比 `Err(...)` 少见（`qc logs` 的
        // `--since … is neither a duration …` 就是这种形状，第一版没覆盖）。
        ".map_err(|",
        // `thiserror` 的 `#[error("…")]`：属性宏**生成** `Display`，正文全是
        // 用户读到的错误文案（`duplicate MCP server name '{0}'`）。这类既不是
        // `println!`、也不是 `Err(...)` 的实参，位置表不收就整批看不见。
        // 注意它不能像别处那样把 `tr()` 嵌进去——`#[error]` 只接受格式串字面量，
        // 真要翻得改成手写 `impl Display`（I18N.md §10.19）。
        "#[error(",
    ]
}

/// 从 `line` 里 `marker` 之后抽字面量；不是字面量开头的返回 `None`。
///
/// `None` 覆盖 `.label(format!(…))`、`.label(tr("…"))` 这些已经走 i18n 的写法——
/// 这正是本门禁想要的结果：**改成 `tr()` 之后它自动不再被算作文案**。
fn literal_after(line: &str, marker: &str) -> Option<String> {
    let rest = line[line.find(marker)? + marker.len()..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => {
                // `\` 后面直接跟换行是**续行**（Rust 会把换行与后面的缩进一起吃掉），
                // 不是转义。不认它，一条多行文案会被切成好几段，基线文件里就出现
                // 只有缩进和 `\x20` 的碎片行——那样的账没法读，也没法还。
                if let Some((_, escaped)) = chars.next() {
                    if escaped == '\n' {
                        while let Some((_, next)) = chars.clone().next() {
                            if next.is_whitespace() {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        out.push(' ');
                        continue;
                    }
                    out.push('\\');
                    out.push(escaped);
                }
            }
            _ => {
                let _ = index;
                out.push(ch);
            }
        }
    }
    None
}

/// 一个文件的 UI 位置字面量。
///
/// `forms.rs` 里的 `=> "…"` 额外算作文案位置：`Page::title()` 是
/// `match self { Self::Welcome => "Welcome", … }`，那 11 个页面名直接进侧栏。
/// **只对 `forms.rs` 开这个位置**——别的文件里 `=> "…"` 大多是状态名与 widget id
/// （`"queue"` / `"ok"` / `"running"`），全算进来就是纯噪声。
fn ui_text_in(relative: &str, production: &str) -> Vec<(usize, String)> {
    // 纯 key 形态（`gui.setup.page.welcome`）不是文案：它是查表的地址。
    // 这一条是为「把 `Page::title()` 改成返回 key」准备的，否则改完本门禁会立刻误报。
    let is_key = |text: &str| {
        !text.is_empty()
            && text.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            })
    };

    // 行首带方括号标记的（`[tool] …`、`[approval] …`、`[warn] …`、`[turn] …`）
    // 是**进度/诊断行**，与 `tracing` 日志同类，有意保持英文（I18N.md §10.15）：
    // 它们的方括号段是给人 `grep '^\[tool\]'` 用的，翻了就没了。这是一条**格式**
    // 规则（首字符是 `[`、内部无空格、紧接 `]`），不是「像不像文案」的猜测。
    let is_progress_tag = |text: &str| {
        let Some(rest) = text.strip_prefix('[') else {
            return false;
        };
        let Some(close) = rest.find(']') else {
            return false;
        };
        let tag = &rest[..close];
        !tag.is_empty()
            && tag
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch == '-')
    };

    // 只剩占位符的（`{report}`、`{HELP}`、`{}`、`{label} {field}`）不是文案：
    // 里面一个"字"都没有，全是变量插值。`println!("{report}")` 这种写法很常见，
    // 它把文案留在 `report` 里、由别处拼好——那部分另有位置可查。
    let is_placeholder_only = |text: &str| {
        let mut rest = text.to_string();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            rest.replace_range(open..open + close + 1, " ");
        }
        // 转义对（`\n`、`\t`）也不是字：`"{}\n{}"` 只是"两个占位符之间换个行"。
        let mut cleaned = String::new();
        let mut iter = rest.chars();
        while let Some(ch) = iter.next() {
            if ch == '\\' {
                let _ = iter.next();
            } else {
                cleaned.push(ch);
            }
        }
        !cleaned.chars().any(|ch| ch.is_ascii_alphabetic())
    };

    // `--- a` / `+++ b` 是 **unified diff 的文件头**，不是文案：`qc config init`
    // 与 `qc config append` 都会先打一段 diff 让人确认，那两个前缀是补丁语法的
    // 一部分，翻了两边就对不上、`patch` 也不认。
    let is_diff_header = |text: &str| text.starts_with("--- ") || text.starts_with("+++ ");

    let mut markers: Vec<&str> = ui_text_positions().to_vec();
    if relative.ends_with("forms.rs") {
        markers.push("=> ");
    }

    // 多行调用的字面量在下一行：`println!(\n    "shell: {} …",`。只看单行会漏掉
    // 整类写法（`qc shell-env` / `qc doctor` 都是这么写的），所以把「本行剩余
    // 部分 + 紧随其后的几行」拼成一段再找。
    let lines: Vec<&str> = production.lines().collect();
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        // 一次调用的字面量常常在**下一行**：
        //     return Err(format!(
        //         "cannot read {}: {error}", dir.display()).into());
        // 第一版的续看条件是「标记正好结束该行」——`Err(format!(` 结束该行但
        // `Err(` 不结束，于是整类漏掉（`shell_env.rs` 的 `"{} exited with …"`、
        // `logs.rs` 的 `cannot read …` 之外还有几处）。判据换成**括号有没有闭合**：
        // 这才是「这个调用还没写完」的真正含义，可判定、不靠行数猜。
        let paren_delta = |text: &str| -> i32 {
            text.chars().filter(|ch| *ch == '(').count() as i32
                - text.chars().filter(|ch| *ch == ')').count() as i32
        };
        let mut end_line = index;
        let mut depth = paren_delta(line);
        while depth > 0 && end_line + 1 < lines.len() && end_line - index < 12 {
            end_line += 1;
            depth += paren_delta(lines[end_line]);
        }
        let tail = if end_line > index {
            lines[index..=end_line].join("\n")
        } else {
            line.to_string()
        };
        // 行级、**作者显式声明**的豁免：`// i18n:keep 机制名，不翻`。
        //
        // 与文件级 `intentional_english` 的区别：那种是「整个文件面向机器」，
        // 这种是「这一行里的字面量是**标识符或值**，不是散文」——`sandbox-exec`
        // 是沙箱后端名（要拿去对 man page 与配置值），`streamable-http` 是协议名。
        // 它们和同文件里真正该翻的错误文案混在一起，文件级豁免会连带盖住后者。
        //
        // 为什么用注释而不是「按内容猜」：判据仍是**位置**——作者标注的位置。
        // 内容猜（「像不像标识符」）会把 `prompt is empty` 也放过。要求理由写在
        // 同一行，是为了让每次豁免都在 diff 里露出来、能被审。
        if tail.contains("// i18n:keep") {
            continue;
        }
        // ADR-0105 §5 划的那条线：工具结果里「发生了什么」跟随界面语言，但
        // `ToolError::*` 的**技术诊断保持英文**——受众是模型，它要据此改参数
        // 重试（`invalid glob`、`invalid hunk header`、`diff is empty`）。
        //
        // `Err(` 这条标记是后加的，把 `Err(ToolError::Invalid("diff is empty"))`
        // 也一并扫了进来；ADR 原文写的是「门禁只认 `ToolOutput::*`，不认
        // `spec(` 与 `ToolError::*`」。所以按位置排掉：整个调用里出现
        // `ToolError::` 的，是在构造诊断，不是给用户读的散文。
        if tail.contains("ToolError::") {
            continue;
        }
        for marker in &markers {
            // `Debug` 实现里的 `write!(f, …)` 是**调试表示**（`Disabled({why})`、
            // `AttachmentId({})`），不是给用户读的文案，跳过。
            //
            // 但 `Display` 实现**不能**跳：`NotInstalled` 那种类型的 `Display`
            // 就是错误正文，`qc daemon` 打到屏幕上给用户看（`qc daemon: {error}`）。
            // 所以判据是「包在 `Debug` 里」，不是「写进 Formatter」——第一版用了
            // 后者，把 `qcd was not found.` 那一整段 actionable 的提示也误杀了。
            if is_in_debug_impl(&lines, index) {
                continue;
            }
            // 两参数的调用点（`self.warn(key, detail)`）**第二个**字面量才是文案：
            // 只取 `literal_after` 会永远看到第一个（也就是已翻好的 key），明细就
            // 整批躲过去。doctor 的 22 个标签与约 45 条明细正是这么漏掉两批的，
            // 所以这几个标记取「标记之后的全部字面量」。
            let texts = if is_multi_literal_marker(marker) {
                literals_after(&tail, marker)
            } else {
                literal_after(&tail, marker).into_iter().collect()
            };
            for text in texts {
                if !text.chars().any(|ch| ch.is_ascii_alphabetic()) {
                    continue;
                }
                if is_key(&text)
                    || is_progress_tag(&text)
                    || is_placeholder_only(&text)
                    || is_diff_header(&text)
                    || is_field_dump_line(&text)
                {
                    continue;
                }
                out.push((index + 1, text));
            }
        }
    }
    out
}

/// 这些标记要取「**整个调用**里的全部字面量」，而不只是紧跟着的那一个。
///
/// 两种调用形状都要覆盖：
/// - `self.ok(label_key, "detail")` —— 第一个字面量是标签，第二个才是明细；
/// - `push_notice(format!("usage: …"))` —— 紧跟着的是 `format!(`，字面量在它里面。
///
/// 第一版只处理了前者（循环调 `literal_after`），于是后者整批漏掉；第一版还因为
/// `literal_after` 要求「标记后立刻是引号」而**一条都没取到**，门禁照样绿——
/// 这两次都是靠反向验证发现的（I18N.md §10.17/§10.18）。
fn is_multi_literal_marker(marker: &str) -> bool {
    matches!(
        marker,
        "self.ok("
            | "self.warn("
            | "self.fail("
            | "doctor.ok("
            | "doctor.warn("
            | "doctor.fail("
            | "push_notice("
            | "write!("
            | "writeln!("
            | ".push_str("
            | "Err("
            | ".ok_or_else("
            | ".ok_or("
            | ".map_err(|"
            | "#[error("
    )
}

/// 是否是「`键=值` 数据列」的一行（`"{id}  turns=3  cost=$0.1  model=x  cwd=/y"`）。
///
/// 与 `qc config show` 同类：字段名要能稳定 grep、能对账，翻了就变成另一套词汇
/// （`turns=3` vs `轮数=3`），而 `qc sessions list` 的输出是给人扫和对账用的。
/// 判据是「含至少两个 `名字=` 片段」——可判定，不是「像不像文案」。
fn is_field_dump_line(text: &str) -> bool {
    // 单个字段追加也算：`"  title={title:?}"`、`"  turns={}"`。判据是「行首
    // （跳空白）就是 `标识符=`」——这是 `按键=值` 数据列的定义，不是猜测。
    let head = text.trim_start();
    if let Some(eq) = head.find('=') {
        let name = &head[..eq];
        if name.len() >= 3
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return true;
        }
    }
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            let mut j = i;
            while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
                j -= 1;
            }
            let name = &text[j..i];
            if name.len() >= 3
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                count += 1;
                if count >= 2 {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// 这一行是否落在某个 `impl … Debug` 里。
///
/// 往回找最近的 `impl ` 头（最多 60 行）看它是不是 `Debug`。不用语法分析：
/// 这些文件里 `impl` 头与它的方法之间不会夹另一个 `impl`。
fn is_in_debug_impl(lines: &[&str], index: usize) -> bool {
    let start = index.saturating_sub(60);
    for line in lines[start..index].iter().rev() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("impl ") || trimmed.starts_with("impl<") {
            return trimmed.contains("Debug");
        }
    }
    false
}

/// `marker` 那次**调用**内部的全部字符串字面量（按出现顺序）。
///
/// 按括号配平截取，而不是「往后扫几行」：后者会把下一个语句的字面量也算进来，
/// 造出无关的基线条目。
fn literals_after(text: &str, marker: &str) -> Vec<String> {
    let Some(at) = text.find(marker) else {
        return Vec::new();
    };
    let body_start = at + marker.len();
    let bytes = text.as_bytes();
    let mut depth = 1i32;
    let mut end = text.len();
    let mut i = body_start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // 跳过字符串（含转义），免得把串里的括号算进配平。
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let mut out = Vec::new();
    let mut chars = text[body_start..end].chars();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut lit = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                // 转义对原样保留；`\` 后跟换行是续行，折成一个空格。
                '\\' => match chars.next() {
                    Some('\n') => {
                        while let Some(w) = chars.clone().next() {
                            if w.is_whitespace() {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        lit.push(' ');
                    }
                    Some(escaped) => {
                        lit.push('\\');
                        lit.push(escaped);
                    }
                    None => break,
                },
                _ => lit.push(c),
            }
        }
        if closed {
            out.push(lit);
        }
    }
    out
}

/// 有意保持英文的文件（每条都带理由，不是「懒得翻」）。/// 有意保持英文的文件（每条都带理由，不是「懒得翻」）。
///
/// 判据只有一条：**它面向的是机器或另一个进程，不是坐在界面前的人。**
/// 想往这里加一行，先在 PR 里写清为什么不翻——这张表是审计面，不是垃圾桶。
///
/// 与基线的区别：基线是**待还的债**（只许减少，最终归零）；这张表是**决定**，
/// 不随时间减少。两者混在一张表里，棘轮就永远降不到零，也就看不出还剩多少。
fn intentional_english(path: &str) -> bool {
    matches!(
        path,
        // `qcd --help` / `--version`：被脚本与 `qc update` 读的表面，且必须在任何
        // 初始化之前就能回答（I18N.md §9 第 1 条已定：保持英文）。
        | "qcd/main.rs"
        // `tracing` 日志与日志配置的诊断：排障与搜 issue 用，与界面语言无关
        // （ADR-0105 §4）。这条是**唯一**允许在日志里出现英文的地方。
        | "qcode-kernel/logging.rs"
        // 品牌名，不翻。
        | "qcode-gui/linux.rs"
        // 沙箱 helper 子进程的 stderr 前缀：面向的是排障的人，且 helper 进程
        // 可能在语言尚未初始化的极早期退出。
        | "qcode-tools/windows_acl.rs"
        // `qc sandbox probe`：沙箱行为的诊断工具，输出是给人**对照 ADR 与
        // `sandbox-exec` 文档**排障用的，不是给终端用户读的。它与 `qc doctor`
        // 不同——doctor 面向用户，最终要翻（在基线里）。
        | "qcode/sandbox_probe.rs"
        // sandbox-exec 的 **profile DSL**（`(allow file-read*` / `(deny
        // file-write*` …）：那是喂给 `sandbox-exec` 的脚本，不是文案。翻一个
        // 括号或关键字，沙箱就整条失效，而且是**静默**失效（`sandbox-exec`
        // 会报语法错并拒跑，但报错信息不指向这里）。这是唯一一个「翻了会坏事
        // 而不是难看」的文件。
        | "qcode-tools/sandbox/macos.rs"
        // `[approval]` 的结构化明细（`argv:` / `cwd:` / `rule:` / `reason:` …）：
        // 与 `qc config show` 同类——`字段名: 值` 的 dump，字段名要能 grep。
        // 注意 `reason` 的值是**模型写的**，它本来就可能是中文，这一块天然混排，
        // 所以更要把字段名固定成一种语言。
        | "qcode-core/approval.rs"
    )
}

/// 门禁 2 的英文面：禁止**新增**硬编码的英文界面文案。
///
/// 与中文那条是同一件事的两个方向。只查中文会漏掉这一类——本门禁上线前，
/// 设置页/向导有 150 多条英文文案从未走过 `tr()`（`forms.rs` 的 `FieldSpec` 数据表
/// 就是主战场），而中文那条门禁对它们完全沉默。
///
/// 基线是**棘轮**：只允许减少。改一条就把基线重抓一次。
#[test]
fn no_new_hardcoded_english_ui_text() {
    let root = workspace_root();
    let baseline_path = root.join("scripts").join("i18n-baseline-en.txt");

    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    // 全 crate 扫描：中文门禁（门禁 2）本来就扫全部 crate，英文面只扫一半的话，
    // `qcode-tools` / `qcode-core` 里新增硬编码英文工具输出就没人管。
    for crate_name in [
        "qcode-gui",
        "qcode-tui",
        "qcode-ui-state",
        "qcode",
        "qcode-tools",
        "qcode-core",
        "qcode-kernel",
        "qcode-mcp",
        "qcode-providers",
        "qcode-terminal",
        "qcode-shell",
        "qcode-driver",
        "qcode-ui-bridge",
        "qcode-attachments",
        "qcode-image",
        "qcd",
    ] {
        let dir = root.join("crates").join(crate_name).join("src");
        for entry in scan_ui_sources(&dir) {
            let (relative, production) = entry;
            if intentional_english(&format!("{crate_name}/{relative}")) {
                continue;
            }
            // `output.rs` 整个文件是有意的英文面（I18N.md §10.15）：
            // `qc config show` 是 `键 = 值` 的 dump（要能 grep/diff，且没有
            // `--json` 旁路），其余是 `[tool]` / `[approval]` / `[turn]` 这种
            // 行首带方括号的进度标记。逐条记进基线只会让真正要还的债看不出来。
            if relative == "output.rs" {
                continue;
            }
            for (_, text) in ui_text_in(&relative, &production) {
                found.insert((format!("{crate_name}/{relative}"), text));
            }
        }
    }

    if std::env::var_os("QCODE_I18N_UPDATE_BASELINE").is_some() {
        let mut text = String::from(
            "# 门禁 2 的英文面基线：现存硬编码英文 UI 文案（I18N.md §8.1）。\n\
             # 格式：crate/相对路径<TAB>字面量。**只允许减少。**\n\
             # 重跑：QCODE_I18N_UPDATE_BASELINE=1 cargo test -p qcode-i18n\n\
             #\n\
             # 判定按**位置**（`.label(` / `.aria_label(` / `group_heading(` /\n\
             # `label:` / `help:` / `title:`，forms.rs 另加 `=> `），不按内容——\n\
             # 「像不像文案」没有可靠判据，见 `ui_text_positions` 的注释。\n",
        );
        for (path, literal) in &found {
            text.push_str(path);
            text.push('\t');
            text.push_str(literal);
            text.push('\n');
        }
        std::fs::write(&baseline_path, text).expect("写不了英文基线文件");
        return;
    }

    let baseline = read_baseline(&baseline_path);
    let added: Vec<_> = found.difference(&baseline).collect();
    let removed: Vec<_> = baseline.difference(&found).collect();

    let mut problems = Vec::new();
    if !added.is_empty() {
        problems.push(format!(
            "新增了 {} 条硬编码英文 UI 文案。界面文案请走 `qcode_i18n::tr()`：",
            added.len()
        ));
        for (path, literal) in added.iter().take(30) {
            problems.push(format!("  + [{path}] {literal:?}"));
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
        for (path, literal) in removed.iter().take(10) {
            problems.push(format!("  - [{path}] {literal:?}"));
        }
        problems.push(
            "确认无误后执行：\n    QCODE_I18N_UPDATE_BASELINE=1 cargo test -p qcode-i18n"
                .to_string(),
        );
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// 遍历一个 crate 的 `src/`，返回 `(相对 `src/` 的路径, 生产部分源码)`。
///
/// 路径**带子目录**：`sandbox/macos.rs` 这种分层不是装饰，豁免表按完整相对路径
/// 点名（`qcode-tools/sandbox/macos.rs`）。第一版只返回 basename，于是**所有分子
/// 目录的豁免都从未生效**（`matches!` 是精确比较），不同的 `mod.rs` 还会在基线与
/// 报错里撞成一个名字——查的人会去错的文件。
fn scan_ui_sources(dir: &Path) -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if matches!(name, "tests" | "target") || name.starts_with('.') {
                    continue;
                }
                walk(root, &path, out);
                continue;
            }
            if !name.ends_with(".rs") || name == "tests.rs" {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let production = &source[..test_module_start(&source)];
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, production.to_string()));
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn ui_text_detector_ignores_translated_calls() {
    // 已经走 tr() 的不再算文案——这是「改完就自动出清单」的关键。
    assert_eq!(
        literal_after(r#"  .label(tr("gui.setup.back"))"#, ".label("),
        None
    );
    assert_eq!(
        literal_after(r#"  .label(format!("{x}"))"#, ".label("),
        None
    );
    // 字面量本身要认出来。
    assert_eq!(
        literal_after(r#"  .label("Back"),"#, ".label(").as_deref(),
        Some("Back")
    );
}

#[test]
fn ui_text_detector_skips_key_shaped_strings() {
    let hits = ui_text_in(
        "forms.rs",
        "fn f() {\n    Self::Welcome => \"gui.page.welcome\",\n}\n",
    );
    assert!(hits.is_empty(), "纯 key 不该算文案：{hits:?}");
    let hits = ui_text_in(
        "forms.rs",
        "fn f() {\n    Self::Welcome => \"Welcome\",\n}\n",
    );
    assert_eq!(hits.len(), 1, "英文页面名要算文案：{hits:?}");
}

#[test]
fn tool_error_diagnostics_stay_english() {
    // ADR-0105 §5：`ToolError::*` 是给模型改参数重试用的技术诊断，不是界面
    // 文案。`Err(` 这条标记会把它们一起扫进来，所以按位置排掉。
    let hits = ui_text_in(
        "qcode-tools/patch.rs",
        "fn f() {\n    return Err(ToolError::Invalid(\"diff is empty\".to_string()));\n}\n",
    );
    assert!(hits.is_empty(), "ToolError 诊断不该进门禁：{hits:?}");

    // 同一条 `Err(` 位置上**不是** ToolError 的仍然要抓：那条豁免不能顺手把
    // 整类「走返回值」的用户错误也放掉（这批正是上一轮刚补回来的）。
    let hits = ui_text_in(
        "qcode-core/todo.rs",
        "fn f() {\n    return Err(format!(\"item {number} has empty `content`\"));\n}\n",
    );
    assert_eq!(hits.len(), 1, "用户读的错误串仍要抓：{hits:?}");
}
