//! 界面语言的唯一决策点与唯一翻译入口（ADR-0105）。
//!
//! # 为什么只有一个 crate 调 `i18n!`
//!
//! `rust-i18n` 4.x 的 `t!` 展开成 `crate::_rust_i18n_t!` —— 绑定**调用方** crate。
//! 所以翻译表无法像 3.x 那样跨 crate 共享，每个用 `t!` 的 crate 都得自己带一份
//! `locales/`。摆在我们面前的是「十份 yml 各写一遍」这种会让 key 漂移的方案，和
//! 「一份 yml + 函数访问器」这个。选后者：翻译只对着一个文件干活（ADR-0105 §8）。
//!
//! 代价是失去 `t!` 的语法糖。`tr()` / `trf()` 就是那层薄封装。
//!
//! # 语言从哪来
//!
//! `QCODE_LANG` → 配置里的显式值 → 系统首选语言 → [`EN`]（ADR-0105 决定 2）。
//!
//! **只读环境变量是不够的**：中文 macOS 上 `LANG` / `LC_ALL` / `LC_MESSAGES` /
//! `LANGUAGE` 可能四个全空，而 Windows 上这些基本不存在。只读 env 等于让中文
//! macOS 用户和全部 Windows 用户永远拿英文。
//!
//! # 谁在什么时候调 [`init`]
//!
//! 必须在 clap 解析**之前**：帮助文案是解析时求值的（`qc::main`）。GUI 那边要在
//! `gpui_kit::init(cx)` 之后调一次，那一次 `set` 同时覆盖框架自带文案和我们自己的
//! —— 两边是同一份 `rust-i18n`，同一个 `CURRENT_LOCALE`。

// 唯一一次 `i18n!`。`locales/` 相对本 crate 的 `CARGO_MANIFEST_DIR`。
rust_i18n::i18n!("locales", fallback = "en");

use std::borrow::Cow;

/// 英文。
pub const EN: &str = "en";

/// 简体中文。繁体在 [`normalize`] 里也归到这里。
pub const ZH_CN: &str = "zh-CN";

/// 全部受支持的 locale。必须与 `locales/app.yml` 的 locale 集合一致（有单测盯）。
pub const SUPPORTED: [&str; 2] = [EN, ZH_CN];

/// 认不出任何线索时用的语言。
///
/// 落 `en` 而不是 `zh-CN` 是**有意的**：项目面向开源，让一个法语用户开箱看到
/// 中文比看到英文更奇怪。与 cot_cli 相反，见 ADR-0105 决定 2。
pub const DEFAULT: &str = EN;

/// 显式覆盖语言的环境变量，优先级最高。
///
/// 命名沿 `QCODE_CONFIG` / `QCODE_DATA_DIR` 的既有约定。脚本与回归用它把输出
/// 钉死，不依赖跑它的机器是什么语言。
pub const LANG_ENV: &str = "QCODE_LANG";

/// 把各种写法归一化成 [`SUPPORTED`] 里的一项；认不出返回 `None`。
///
/// **必须归一化，不能把系统返回值直接交给 `set_locale`**：macOS 的
/// `CFLocaleCopyPreferredLanguages` 返回 `zh-Hans-CN`（带 script 子标签），
/// rust-i18n 查不到该 locale 会**静默** fallback 到英文，既不报错也不警告。
///
/// 步骤：切掉编码（`.UTF-8`）与修饰符（`@euro`）→ `_` 换 `-` → 转小写 →
/// 只比较**第一个 subtag**（所以 `zho` 不会被误判成 `zh`）。
pub fn normalize(raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_modifier = trimmed.split(['.', '@']).next().unwrap_or("");
    let lowered = without_modifier.replace('_', "-").to_lowercase();
    match lowered.split('-').next().unwrap_or("") {
        // Linux CI 的典型值：`LANG=C.UTF-8` 在上面被切成 `c`。
        "c" | "posix" => Some(EN),
        "en" => Some(EN),
        // 繁体（zh-Hant / zh-TW / zh-HK）也走简体：只有一份中文译文，对繁体用户
        // 来说中文仍比英文友好。
        "zh" => Some(ZH_CN),
        _ => None,
    }
}

/// 纯决策：显式值优先；显式值认不出就继续往下看（不报错）；都没有则 [`DEFAULT`]。
///
/// 显式值认不出时**静默下探**而不是报错：一个写错的 `QCODE_LANG` 不该让程序
/// 起不来。
pub fn decide(explicit: Option<&str>, system: Option<&str>) -> &'static str {
    explicit
        .and_then(normalize)
        .or_else(|| system.and_then(normalize))
        .unwrap_or(DEFAULT)
}

/// 唯一的副作用入口。在 clap 解析**之前**调用一次（见模块文档）。
///
/// `configured` 是调用方已经从配置里读出的显式值（`[ui] language`）。
/// 优先级：`QCODE_LANG` > `configured` > 系统首选 > [`DEFAULT`] —— 环境变量压过
/// 配置文件，与 `RUST_LOG` 压过 `[log] level` 同一条规矩（ADR-0082）。
pub fn init(configured: Option<&str>) {
    let from_env = std::env::var(LANG_ENV).ok();
    let explicit = from_env.as_deref().or(configured);
    // 只取系统首选语言，不扫描整个优先级列表：首选是法语的用户给他中文更奇怪。
    let system = sys_locale::get_locale();
    set(decide(explicit, system.as_deref()));
}

/// 归一化后写入。认不出时落 [`DEFAULT`]。
///
/// 这是进程级状态（`rust-i18n` 的 `CURRENT_LOCALE` 是一个全局 `AtomicStr`）。
pub fn set(locale: &str) {
    rust_i18n::set_locale(normalize(locale).unwrap_or(DEFAULT));
}

/// 当前生效的语言。
///
/// 取自 `rust-i18n` 本体而不是自己再存一份，避免「我们以为的语言」与「实际翻译用
/// 的语言」两处状态不一致。
pub fn locale() -> String {
    normalize(&rust_i18n::locale())
        .unwrap_or(DEFAULT)
        .to_string()
}

/// 取一条译文。key 见 `locales/app.yml`。
///
/// 缺 key 时 `rust-i18n` 不报错，只把 key（或 `locale.key`）原样吐出来 —— 靠
/// 门禁（key 集合相等）兜，不靠祈祷。
/// 取一条译文，key 允许是**运行期拼出来的**。取不到返回 `None`。
///
/// 与 [`tr`] 的唯一区别是 key 的存活期。`tr` 收 `&'static str`：调用点上是字面量，
/// 门禁 4（引用的 key 存在）与门禁 6（声明的 key 有人用）都能静态看见。这一条收
/// `&str`，专给**结构化拼出来**的 key 用——目前只有一处：clap 的帮助树按
/// `cli.cmd.<命令路径>.<字段>` 查表（见 `crates/qcode/src/clap_i18n.rs`）。
///
/// 这类 key 静态扫描看不见，所以另配一道**可达性门禁**
/// （`crates/qcode/tests/cli_i18n.rs`）：遍历真实的 `Command` 树，断言每个查表点
/// 都命中一条声明的 key，且每条 `cli.cmd.*` 都能被查到。否则就只能靠人眼。
pub fn try_tr(key: &str) -> Option<Cow<'static, str>> {
    try_tr_in(&locale(), key)
}

/// 同 [`try_tr`]，locale 显式指定（测试用）。
pub fn try_tr_in(locale: &str, key: &str) -> Option<Cow<'static, str>> {
    let text = _rust_i18n_translate(locale, key);
    // 取不到时 rust-i18n 返回 `<locale>.<key>`：同一个哨兵门禁 5 也在用。
    if text == format!("{locale}.{key}") {
        return None;
    }
    // `text` 的 `Cow` 可能借着 `key` 的存活期（哨兵分支已排除，这里是命中分支，
    // 底层返回的是嵌入二进制的 `&'static str`），显式声明成 `'static`。
    Some(Cow::Owned(text.into_owned()))
}

pub fn tr(key: &'static str) -> Cow<'static, str> {
    _rust_i18n_translate(&locale(), key)
}

/// 取一条译文并填 `%{name}` 占位符。
///
/// ```ignore
/// trf("gui.terminal.max", &[("max", "4")]);
/// ```
pub fn trf(key: &'static str, args: &[(&str, &str)]) -> String {
    replace(
        key,
        args.iter().map(|(name, _)| *name),
        args.iter().map(|(_, v)| *v),
    )
}

/// 取一条译文并按**位置**填 `%{0}`、`%{1}` … 占位符。
///
/// 这是给 `format!("最多 {} 个…", n)` 这类迁移用的：原来第 n 个 `{}` 换成
/// `%{n}`，调用点保持参数顺序，不必给每个洞起名字。译文里写 `%{0}`、`%{1}`，
/// 顺序由译文自己决定（英文与中文的语序常常不同）。
pub fn trp<S: AsRef<str>>(key: &'static str, args: &[S]) -> String {
    trp_in(&locale(), key, args)
}

/// 同 [`trp`]，但 locale 显式指定。
///
/// 给两类调用方：**测试**（要同时看到两种语言的形态，又不能碰进程级的
/// `CURRENT_LOCALE`，见 ADR-0105 §6），以及需要为「非当前界面语言」产出文本的
/// 地方（例如给另一种语言写文件）。
pub fn trp_in<S: AsRef<str>>(locale: &str, key: &'static str, args: &[S]) -> String {
    let names: Vec<String> = (0..args.len()).map(|index| index.to_string()).collect();
    let values: Vec<String> = args.iter().map(|arg| arg.as_ref().to_string()).collect();
    rust_i18n::replace_patterns(
        &_rust_i18n_translate(locale, key),
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
        &values,
    )
}

fn replace<'a>(
    key: &'static str,
    patterns: impl Iterator<Item = &'a str>,
    values: impl Iterator<Item = &'a str>,
) -> String {
    let template = tr(key).into_owned();
    let patterns: Vec<&str> = patterns.collect();
    let values: Vec<String> = values.map(str::to_string).collect();
    rust_i18n::replace_patterns(&template, &patterns, &values)
}

/// 把进程语言钉到 [`ZH_CN`]，**只给测试用**。
///
/// 测试进程从不调用 [`init`]（那是 `main` 的事），于是语言会停在 rust-i18n 的
/// 初值 `en`。而现有那一批断言期待的是中文文案，所以测试必须自己钉（ADR-0105 §6）。
///
/// 为什么钉中文而不是英文：现有 ≈93 处断言期待中文，钉中文 → 一行都不用改；
/// 钉英文 → 那 93 处全要改。「英文路径正确」不靠断言，靠三道门禁。
///
/// 为什么可以随便调：值恒定、幂等，用 `Once` 兜一层，并发调也只有一个赢家，
/// 不产生竞争。**不要**用它在单个测试里来回切换语言 —— `CURRENT_LOCALE` 是
/// 进程级的，并行测试会互相污染。
pub fn pin_for_tests() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| set(ZH_CN));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 归一化全表。`zh-Hans-CN` 是中文 macOS 的真实返回值，必须覆盖。
    #[test]
    fn normalize_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("", None),
            ("   ", None),
            (".UTF-8", None),
            ("c", Some(EN)),
            ("C", Some(EN)),
            ("C.UTF-8", Some(EN)),
            ("POSIX", Some(EN)),
            ("en", Some(EN)),
            ("en-US", Some(EN)),
            ("en_US.UTF-8", Some(EN)),
            ("en-CN", Some(EN)),
            ("zh", Some(ZH_CN)),
            ("zh-CN", Some(ZH_CN)),
            ("zh_CN.UTF-8", Some(ZH_CN)),
            ("zh-Hans", Some(ZH_CN)),
            // macOS 的 `CFLocaleCopyPreferredLanguages` 就返回这个。
            ("zh-Hans-CN", Some(ZH_CN)),
            ("zh-Hant", Some(ZH_CN)),
            ("zh-TW", Some(ZH_CN)),
            ("zh-HK", Some(ZH_CN)),
            ("zh-Hans-CN@euro", Some(ZH_CN)),
            ("fr", None),
            ("fr-FR", None),
            ("de-DE", None),
            ("ja-JP", None),
            // 按 subtag 边界比较：`zho` 不是 `zh`。
            ("zho", None),
        ];
        for (input, want) in cases {
            assert_eq!(normalize(input), *want, "normalize({input:?})");
        }
    }

    /// 决策矩阵（I18N.md §4.4）。
    #[test]
    fn decide_matrix() {
        // 系统侧。
        assert_eq!(decide(None, Some("zh-Hans-CN")), ZH_CN);
        assert_eq!(decide(None, Some("en-CN")), EN);
        assert_eq!(decide(None, Some("C.UTF-8")), EN);
        assert_eq!(decide(None, Some("fr-FR")), EN);
        assert_eq!(decide(None, None), EN);
        // 显式侧优先。
        assert_eq!(decide(Some("en"), Some("zh-Hans-CN")), EN);
        assert_eq!(decide(Some("zh-CN"), Some("en-CN")), ZH_CN);
        // 显式值认不出时静默往下走，不报错。
        assert_eq!(decide(Some("fr"), Some("zh-Hans-CN")), ZH_CN);
        assert_eq!(decide(Some("fr"), None), EN);
        // 空值当未设置。
        assert_eq!(decide(Some(""), Some("zh-Hans-CN")), ZH_CN);
    }

    /// 译文按 locale 取，绕过全局状态（并行测试里不能靠 `set`）。
    #[test]
    fn translations_resolve_per_locale() {
        assert_eq!(_rust_i18n_translate(EN, "app.name"), "qcode");
        assert_eq!(_rust_i18n_translate(ZH_CN, "app.name"), "qcode");
        // 这一条两个语言不同，才真正证明按 locale 分支。
        assert_eq!(_rust_i18n_translate(EN, "app.error_prefix"), "Error: ");
        assert_eq!(_rust_i18n_translate(ZH_CN, "app.error_prefix"), "错误：");
    }

    /// `SUPPORTED` 必须与 rust-i18n 实际加载到的 locale 集合一致，否则会出现
    /// 「设了某个 locale 但没有任何译文」的静默失效。
    #[test]
    fn supported_matches_available_locales() {
        let mut available = rust_i18n::available_locales!();
        available.sort();
        let mut expected = SUPPORTED.to_vec();
        expected.sort();
        assert_eq!(
            available, expected,
            "SUPPORTED 与 locales/app.yml 的 locale 集合不一致"
        );
    }

    /// 钉盘之后 `locale()` 报中文（ADR-0105 §6）。
    #[test]
    fn pin_for_tests_lands_on_chinese() {
        pin_for_tests();
        assert_eq!(locale(), ZH_CN);
    }
}
