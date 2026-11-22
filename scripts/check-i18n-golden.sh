#!/usr/bin/env bash
# 门禁 3：双语 golden（I18N.md §8.1）。
#
# 把命令树里**每一个**子命令的 `--help` 与两种典型报错，在 `QCODE_LANG=en` 与
# `QCODE_LANG=zh-CN` 下各抓一遍，和基线逐字节比。
#
#   - `en` 那份必须**零差异**（不变量 1：英文输出与双语化之前逐字节相同）。
#   - `zh-CN` 那份随翻译批次正常变化，每批完成后重抓基线。
#
# 用法：
#   scripts/check-i18n-golden.sh            # 检查（默认读 target/debug/qc）
#   scripts/check-i18n-golden.sh --record   # 重抓基线
#   QC_BIN=/path/to/qc scripts/check-i18n-golden.sh
#
# 为什么不把 `--version` 入基线：它内嵌 `git describe`，HEAD 一前进就变（与文案
# 无关）。它只断言「两种语言下逐字节相同」——那才是不变量。
#
# 已知限制：基线是**单一平台形状**。`qc --help` 里有个 Windows-only 且不 hidden 的
# `sandbox` 子命令，所以在 Windows 上跑这条门禁会因为顶层多一行而失败。门禁 3 只在
# `make check` 与 release 里跑（不进 CI 矩阵），目前不算问题；真要跨平台跑，得先决定
# 平台相关命令怎么进基线。

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
QC=${QC_BIN:-"$ROOT/target/debug/qc"}
BASELINE="$ROOT/scripts/i18n-golden"
LANGS=(en zh-CN)

# 深度上限只是防跑飞；现在最深是 `config key set`（三层）。
MAX_DEPTH=4

record=0
case "${1:-}" in
--record) record=1 ;;
-h | --help)
    sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
"") ;;
*)
    echo "error: 未知参数 $1（只接受 --record）" >&2
    exit 2
    ;;
esac

[ -x "$QC" ] || {
    echo "error: 找不到可执行文件 $QC" >&2
    echo "       先 cargo build -p qcode，或用 QC_BIN=... 指定" >&2
    exit 2
}

# 每次运行都用一个全新的临时目录，绝不碰用户真实的数据目录与配置。
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# 固定抓取环境：无颜色（转义码会让 diff 不可读）、固定 TERM、隔离数据目录。
run_qc() {
    local lang=$1
    shift
    env -i \
        PATH="$PATH" \
        HOME="$work/home" \
        TERM="xterm-256color" \
        NO_COLOR=1 \
        QCODE_LANG="$lang" \
        QCODE_DATA_DIR="$work/data-$lang" \
        QCODE_CONFIG="$work/config-$lang.toml" \
        "$QC" "$@" 2>&1
}

# 版本号在 `--version` 之外也会出现在帮助里（`qc <ver>`），从基线里抹平。
capture() {
    local lang=$1
    shift
    run_qc "$lang" "$@" | sed -E 's/\bqc [0-9]+\.[0-9]+\.[0-9]+[^ ]*/qc <version>/g'
}

fail=0
checked=0

# 一个用例：跑一遍、与基线比对（或重抓）。
check_case() {
    local lang=$1 name=$2
    shift 2
    local out
    out=$(capture "$lang" "$@")
    local file="$BASELINE/$lang/$name.txt"
    if [ "$record" = 1 ]; then
        printf '%s\n' "$out" >"$file"
    elif ! diff -u "$file" <(printf '%s\n' "$out") >"$work/diff" 2>&1; then
        echo "✗ [$lang] $name 与基线不一致" >&2
        sed 's/^/    /' "$work/diff" >&2
        fail=1
    fi
    checked=$((checked + 1))
}

# 递归发现子命令，**从二进制自己的 `--help` 里读**。
#
# 这里以前是一张手写的 `SUBCOMMANDS` 列表，而那张列表就是这笔债本身：加一个子命令
# 不会有人记得回来加一行，于是 golden 悄悄不覆盖它——`sessions delete` 与
# `config init` 的 `long_about` 就这样在 P2 里被截断了却没人发现。命令树是 `--help`
# 已经在输出的东西，让它自描述，列表就不会腐坏。
#
# 读的是 `Commands:` 节里「两个空格 + 小写名字」的行；`help` 由 clap 自动加，跳过。
#
# **发现永远用 `en` 跑**，哪怕正在抓的是 `zh-CN`：解析锚点就是英文的 `Commands:`。
# 第一版把当前语言传了进来，于是 `zh-CN` 那轮一个子命令都没发现到（中文那边是
# 「命令：」），48 项里 zh 只占 4 项——覆盖静悄悄地少了一半，而门禁照样报「通过」。
# 拿英文帮助当锚点也正是最稳的选择：英文输出由不变量 1 钉着逐字节不变。
subcommands_of() {
    shift
    run_qc en "$@" --help |
        sed -n '/^Commands:/,/^$/p' |
        sed -n 's/^  \([a-z][a-z0-9-]*\)  .*/\1/p'
}

# 遍历整棵树。`prefix` 是基线文件名前缀，`path` 是走到这里的命令行参数。
walk_help() {
    local lang=$1 prefix=$2 depth=$3
    shift 3
    [ "$depth" -gt "$MAX_DEPTH" ] && return
    local sub
    for sub in $(subcommands_of "$lang" "$@"); do
        # `qc help` 是 clap 自动生成的，内容就是顶层帮助。
        [ "$sub" = "help" ] && continue
        check_case "$lang" "$prefix$sub" "$@" "$sub" --help
        walk_help "$lang" "$prefix$sub-" "$((depth + 1))" "$@" "$sub"
    done
}

for lang in "${LANGS[@]}"; do
    if [ "$record" = 1 ]; then
        mkdir -p "$BASELINE/$lang"
    fi

    # 顶层：`--help` 与 `-h` 两种都抓（`-h` 是短帮助，形状不同）。
    check_case "$lang" help-top
    check_case "$lang" help-top-short -h

    # 整棵子命令树。
    walk_help "$lang" help- 0

    # 两种典型报错：未知全局参数、未知子命令。
    check_case "$lang" err-unknown-flag --nope
    check_case "$lang" err-unknown-sub definitely-not-a-command
done

# 不变量：`--version` 与语言无关（ADR-0105 不变量 2）。
en_version=$(run_qc en --version)
zh_version=$(run_qc zh-CN --version)
if [ "$en_version" != "$zh_version" ]; then
    echo "✗ \`--version\` 被语言影响了（它必须与语言无关）" >&2
    echo "    en   : $en_version" >&2
    echo "    zh-CN: $zh_version" >&2
    fail=1
fi
checked=$((checked + 1))

if [ "$record" = 1 ]; then
    echo "已重抓基线到 ${BASELINE}（${checked} 项）"
    exit 0
fi

if [ "$fail" != 0 ]; then
    echo "" >&2
    echo "门禁 3 失败。确认差异是预期之后重抓基线：" >&2
    echo "    scripts/check-i18n-golden.sh --record" >&2
    exit 1
fi

echo "门禁 3 通过（$checked 项）"
