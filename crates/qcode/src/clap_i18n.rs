//! clap 的帮助与报错文案本地化。
//!
//! # 为什么需要两个机制
//!
//! clap 有两类字符串，来源完全不同：
//!
//! 1. **我们自己的文案** —— `cli.rs` 里的 `///` 注释，derive 会把它当 `about` /
//!    `help`。它能在**构建命令时**被替换（[`localize`]）。
//! 2. **clap 内置的标签** —— `Usage:` / `Options:` / `Print help` / `error:` …。
//!    它们**硬编码在 `clap_builder` 里，没有任何公开 API 可配**，只能等渲染成文本
//!    之后再改写（[`apply_labels`]）。
//!
//! 两个机制都只在**非英文**时生效：英文路径仍然把 clap 原生的 `StyledStr` 直接
//! 打到终端，字节与颜色都不变（I18N.md 不变量 1、§4.5）。
//!
//! # 为什么 key 是拼出来的
//!
//! 帮助文案挂在命令树上，键自然就是**路径**：`cli.cmd.schedule.create.about`、
//! `cli.cmd.run.arg.provider.help`。手写 113 条 `tr("…")` 字面量既容易漏、又会在
//! 加一个子命令时静默忘记。所以按路径查表（[`qcode_i18n::try_tr`]），并由
//! `crates/qcode/tests/cli_i18n.rs` 遍历**真实的** `Command` 树来证明两件事：
//! 每个查表点都命中一条声明的 key，每条 `cli.cmd.*` 都真的会被查到。
//!
//! 查不到时**保留英文**（新加的参数不至于因为忘了写译文就变成一串 key）。代价是
//! 「忘了写译文」本身不会报错——这正是那道可达性门禁存在的理由。

use clap::Command;

/// 把命令树里所有我们自己的文案换成当前语言。
///
/// 英文下等价于空操作（`try_tr` 拿到英文原文，与 `///` 一致），但仍然会跑一遍：
/// 「同一棵命令树」这个前提在两种语言下都成立，调用处不必分叉。
pub(crate) fn localize(cmd: Command) -> Command {
    localize_at(cmd, "")
}

fn localize_at(mut cmd: Command, path: &str) -> Command {
    // 顶层节点的路径写作 `top`：`cli.cmd.top.about` 比 `cli.cmd..about` 可读。
    let node = if path.is_empty() { "top" } else { path };

    if let Some(text) = qcode_i18n::try_tr(&format!("cli.cmd.{node}.about")) {
        cmd = cmd.about(text.into_owned());
    }
    if let Some(text) = qcode_i18n::try_tr(&format!("cli.cmd.{node}.long_about")) {
        cmd = cmd.long_about(text.into_owned());
    }

    cmd = cmd.mut_args(|arg| {
        let id = arg.get_id().to_string();
        match qcode_i18n::try_tr(&format!("cli.cmd.{node}.arg.{id}.help")) {
            // `help` / `version` 是 clap 自己加的参数，没有我们的话要说。
            Some(text) => arg.help(text.into_owned()),
            None => arg,
        }
    });

    // 先收名字再递归：`get_subcommands` 借着 `cmd`，而循环体要移动 `cmd`。
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        let next = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        cmd = cmd.mut_subcommand(name.as_str(), |sub| localize_at(sub, &next));
    }
    cmd
}

/// 一条改写规则。**英文模式在代码里**（它是 clap 的契约，不是我们的文案），
/// **中文在 yml 里**（`cli.label.*`）——否则这里就成了第二份硬编码文案表。
struct Rule {
    kind: Kind,
    /// clap 输出的英文形状。`{}` 是洞（见 [`one_hole`]）。
    pattern: &'static str,
    /// 译文条目（见 [`label_text`]）。
    label: Label,
}

/// 规则表里的译文条目。
///
/// 写成枚举而不是直接放 key 字符串，是为了让 13 条 `cli.label.*` **以字面量的
/// 形式出现在调用点上**——`label_text` 的每个 arm 都是 `tr("cli.label.xxx")`，
/// 门禁 6（声明的 key 有人引用）才看得见。把 key 存进表里再动态查，
/// 静态扫描就完全瞎了。
#[derive(Clone, Copy)]
enum Label {
    Usage,
    Commands,
    Options,
    Arguments,
    PrintHelpFull,
    PrintVersion,
    PrintHelp,
    ErrorPrefix,
    TipPrefix,
    MoreInfo,
    UnexpectedArgument,
    UnrecognizedSubcommand,
}

impl Label {
    fn text(self) -> std::borrow::Cow<'static, str> {
        use qcode_i18n::tr;
        match self {
            Self::Usage => tr("cli.label.usage"),
            Self::Commands => tr("cli.label.commands"),
            Self::Options => tr("cli.label.options"),
            Self::Arguments => tr("cli.label.arguments"),
            Self::PrintHelpFull => tr("cli.label.print_help_full"),
            Self::PrintVersion => tr("cli.label.print_version"),
            Self::PrintHelp => tr("cli.label.print_help"),
            Self::ErrorPrefix => tr("cli.label.error_prefix"),
            Self::TipPrefix => tr("cli.label.tip_prefix"),
            Self::MoreInfo => tr("cli.label.more_info"),
            Self::UnexpectedArgument => tr("cli.label.unexpected_argument"),
            Self::UnrecognizedSubcommand => tr("cli.label.unrecognized_subcommand"),
        }
    }
}

#[derive(PartialEq)]
enum Kind {
    /// 节标题。命中后**额外吃掉后面一个半角空格**：英文是 `Usage: qc`，中文
    /// 全角冒号后面不该留空格（`用法：qc`）；而 `Options:` 独占一行时本来就没空格。
    Heading,
    /// 任意位置子串。
    Anywhere,
    /// 行首锚定且整行替换掉模式本身（前缀 + 余下原文）。
    Prefix,
    /// 含一处 `{}` 洞，双向包裹。
    OneHole,
}

/// 渲染一份**已本地化**的帮助文本。
///
/// `qc --help` 走 clap 自己的错误通道（`DisplayHelp`），但裸 `qc` 是主动打印用法
/// （ADR-0089），那条路径自己调 `render_help`。两处必须产出同一份文字，否则
/// 「`qc` 和 `qc --help` 说的不一样」——所以统一走这里。
///
/// 英文下不套 [`apply_labels`]：`en` 路径要求与改动前逐字节相同（不变量 1）。
pub(crate) fn help_text(cmd: Command) -> String {
    let rendered = localize(cmd).render_help().to_string();
    if qcode_i18n::locale() == qcode_i18n::DEFAULT {
        rendered
    } else {
        apply_labels(&rendered)
    }
}

/// **顺序敏感**：长句必须在短句之前。`Print help (see more with '--help')` 含裸的
/// `Print help`，先被短规则截断就会留下半句英文（I18N.md §4.5 坑 1）。
const RULES: &[Rule] = &[
    // ---- 节标题（行首锚定）----
    Rule {
        kind: Kind::Heading,
        pattern: "Usage:",
        label: Label::Usage,
    },
    Rule {
        kind: Kind::Heading,
        pattern: "Commands:",
        label: Label::Commands,
    },
    Rule {
        kind: Kind::Heading,
        pattern: "Options:",
        label: Label::Options,
    },
    Rule {
        kind: Kind::Heading,
        pattern: "Arguments:",
        label: Label::Arguments,
    },
    // ---- 长句在前 ----
    Rule {
        kind: Kind::Anywhere,
        pattern: "Print this message or the help of the given subcommand(s)",
        label: Label::PrintHelpFull,
    },
    Rule {
        kind: Kind::Anywhere,
        pattern: "Print version",
        label: Label::PrintVersion,
    },
    Rule {
        kind: Kind::Anywhere,
        pattern: "Print help",
        label: Label::PrintHelp,
    },
    // ---- 报错前缀与统一尾句 ----
    Rule {
        kind: Kind::Prefix,
        pattern: "error: ",
        label: Label::ErrorPrefix,
    },
    Rule {
        kind: Kind::Prefix,
        pattern: "tip: ",
        label: Label::TipPrefix,
    },
    Rule {
        kind: Kind::Anywhere,
        pattern: "For more information, try '--help'.",
        label: Label::MoreInfo,
    },
    // ---- 报错正文：只覆盖 golden 里出现的那两种形状 ----
    //
    // 其余 clap 报错（缺必填、值非法、冲突…）的**正文仍是英文**，只拿到中文的
    // `错误：` 前缀与尾句。这是有意的收口：那些句子的中英词序差异大（`invalid
    // value 'X' for 'Y'` 直译会把 Y 留在句尾），用规则表硬拼只会拼出别扭的中文。
    // 记在 I18N.md 的残留清单里。
    Rule {
        kind: Kind::OneHole,
        pattern: "unexpected argument '{}' found",
        label: Label::UnexpectedArgument,
    },
    Rule {
        kind: Kind::OneHole,
        pattern: "unrecognized subcommand '{}'",
        label: Label::UnrecognizedSubcommand,
    },
];

/// 把 clap 渲染出来的纯文本过一遍规则表。
///
/// 入参必须是 **colorless 纯文本**（`Error::render().to_string()` 等）；带 ANSI
/// 转义的话行首锚定会失配。
pub(crate) fn apply_labels(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        out.push_str(&rewrite_line(line));
    }
    out
}

fn rewrite_line(line: &str) -> String {
    let mut owned = line.to_string();
    for rule in RULES {
        let to = rule.label.text();
        let to = to.as_ref();
        owned = match rule.kind {
            Kind::Heading => match owned.strip_prefix(rule.pattern) {
                Some(rest) => format!("{to}{}", join_after_heading(to, rest)),
                None => owned,
            },
            Kind::Anywhere => owned.replace(rule.pattern, to),
            Kind::Prefix => match owned.strip_prefix(rule.pattern) {
                Some(rest) => format!("{to}{rest}"),
                None => owned,
            },
            Kind::OneHole => one_hole(&owned, rule.pattern, to).unwrap_or(owned),
        };
    }
    owned
}

/// 节标题译文与它后面那段原文怎么接。
///
/// 英文 `Usage: qc` 里冒号后**必须**有那个空格；中文全角冒号后**不该**有。
/// 判据因此放在**译文**上，而不是靠调用方「英文别走这条路径」那样的 `if` 守卫——
/// 后者一旦有人为了「统一渲染路径」删掉，英文格式就静默坏掉（`Usage:qc`），
/// 而门禁只抓文本时未必察觉。写成这样，两种语言都是构造上正确的。
fn join_after_heading<'a>(translation: &str, rest: &'a str) -> &'a str {
    // 全角冒号（以及别的全角标点结尾）不需要补半角空格。
    if translation.ends_with('：') || translation.ends_with('。') {
        rest.trim_start_matches(' ')
    } else {
        rest
    }
}

/// 按 `pattern` 里唯一那个 `{}` 把 `line` 切成 前/洞/后，再用 `to` 拼回去。
///
/// 洞取「到下一段固定文本为止」的最短匹配——clap 这类句子都是「一段可变、两侧
/// 固定」。两侧都没命中就返回 `None`，调用方保留原文。
fn one_hole(line: &str, pattern: &str, to: &str) -> Option<String> {
    let (head, tail) = pattern.split_once("{}")?;
    let start = line.find(head)?;
    let rest = &line[start + head.len()..];
    let end = if tail.is_empty() {
        rest.len()
    } else {
        rest.find(tail)?
    };
    let hole = &rest[..end];
    let (to_head, to_tail) = to.split_once("{}").unwrap_or((to, ""));
    let mut out = String::with_capacity(line.len() + 16);
    out.push_str(&line[..start]);
    out.push_str(to_head);
    out.push_str(hole);
    out.push_str(to_tail);
    // 洞与它的固定尾巴之后的一切（换行、后续说明）原样留下。
    let consumed = head.len() + hole.len() + tail.len();
    out.push_str(&line[start + consumed..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `one_hole` 曾经写错过一次（把洞后的尾巴拼了两遍）。这条钉住形状。
    #[test]
    fn one_hole_keeps_the_trailer_exactly_once() {
        let out = one_hole(
            "x unexpected argument '--nope' found y",
            "unexpected argument '{}' found",
            "未识别的参数 '{}'",
        );
        assert_eq!(out.as_deref(), Some("x 未识别的参数 '--nope' y"));
    }

    /// 模式两侧都固定时不越界。
    #[test]
    fn one_hole_with_both_anchors() {
        let out = one_hole(
            "bad: silly is not a valid value",
            "bad: {} is not a valid value",
            "错：{} 不合法",
        );
        assert_eq!(out.as_deref(), Some("错：silly 不合法"));
    }

    /// 模式没有右侧固定文本时吃到行尾。
    #[test]
    fn one_hole_without_tail_takes_the_rest() {
        let out = one_hole("bad: silly\n", "bad: {}", "错：{}");
        assert_eq!(out.as_deref(), Some("错：silly\n"));
    }

    /// 不命中就交回 `None`，调用方保留原文。
    #[test]
    fn one_hole_misses_cleanly() {
        assert!(one_hole("nothing here", "unexpected argument '{}' found", "x").is_none());
    }

    /// 节标题的接法：全角冒号吃掉空格，半角冒号保留它。
    ///
    /// 这条钉的是「英文走 `apply_labels` 也不会变成 `Usage:qc`」——那曾经是个
    /// 靠调用方 if 守卫才成立的隐雷（见 I18N.md §10.11）。
    #[test]
    fn heading_join_depends_on_the_translation_not_the_caller() {
        assert_eq!(
            join_after_heading("用法：", " qc [OPTIONS]"),
            "qc [OPTIONS]"
        );
        assert_eq!(
            join_after_heading("Usage:", " qc [OPTIONS]"),
            " qc [OPTIONS]"
        );
        // 独占一行的节标题（`Options:` 后面什么都没有）两种都成立。
        assert_eq!(join_after_heading("选项：", ""), "");
        assert_eq!(join_after_heading("Options:", ""), "");
    }
}
