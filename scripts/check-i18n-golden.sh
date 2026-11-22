#!/usr/bin/env bash
# 门禁 3：双语 golden（I18N.md §8.1）。
#
# 把全部子命令的 `--help` 与两种典型报错，在 `QCODE_LANG=en` 与 `QCODE_LANG=zh-CN`
# 下各抓一遍，和基线逐字节比。
#
#   - `en` 那份的差异**只允许**是 ADR-0105 §1 记的框架 bug 修复。
#   - `zh-CN` 那份随翻译批次正常变化，每批完成后重抓基线。
#
# 用法：
#   scripts/check-i18n-golden.sh            # 检查（默认读 target/debug/qc）
#   scripts/check-i18n-golden.sh --record   # 重抓基线
#   QC_BIN=/path/to/qc scripts/check-i18n-golden.sh
#
# 为什么不把 `--version` 入基线：它内嵌 `git describe`，HEAD 一前进就变（与文案
# 无关）。它只断言「两种语言下逐字节相同」——那才是不变量。

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
QC=${QC_BIN:-"$ROOT/target/debug/qc"}
BASELINE="$ROOT/scripts/i18n-golden"
LANGS=(en zh-CN)

# `shell-env` 之后是 `help`，它由 clap 自动生成，也跟着一起抓。
SUBCOMMANDS=(
  tui run resume sessions schedule daemon skills logs config template
  mcp doctor shell-env update acp
)

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

# 一个用例 = 名字 + 参数列表。名字用于基线文件名。
capture() {
    local lang=$1 name=$2
    shift 2
    run_qc "$lang" "$@" | sed -E 's/\bqc [0-9]+\.[0-9]+\.[0-9]+[^ ]*/qc <version>/g'
}

fail=0
checked=0

for lang in "${LANGS[@]}"; do
    dir="$BASELINE/$lang"
    if [ "$record" = 1 ]; then
        mkdir -p "$dir"
    fi

    # 顶层 help
    for name_and_args in "help-top:" "help-top-short:-h"; do
        name=${name_and_args%%:*}
        args=${name_and_args#*:}
        out=$(capture "$lang" "$name" ${args:+"$args"})
        file="$dir/$name.txt"
        if [ "$record" = 1 ]; then
            printf '%s\n' "$out" >"$file"
        elif ! diff -u "$file" <(printf '%s\n' "$out") >"$work/diff" 2>&1; then
            echo "✗ [$lang] $name 与基线不一致" >&2
            sed 's/^/    /' "$work/diff" >&2
            fail=1
        fi
        checked=$((checked + 1))
    done

    # 每个子命令的 --help
    for sub in "${SUBCOMMANDS[@]}"; do
        out=$(capture "$lang" "help-$sub" "$sub" --help)
        file="$dir/help-$sub.txt"
        if [ "$record" = 1 ]; then
            printf '%s\n' "$out" >"$file"
        elif ! diff -u "$file" <(printf '%s\n' "$out") >"$work/diff" 2>&1; then
            echo "✗ [$lang] help-$sub 与基线不一致" >&2
            sed 's/^/    /' "$work/diff" >&2
            fail=1
        fi
        checked=$((checked + 1))
    done

    # 两种典型报错：未知全局参数、未知子命令。
    for name_and_args in "err-unknown-flag:--nope" "err-unknown-sub:definitely-not-a-command"; do
        name=${name_and_args%%:*}
        args=${name_and_args#*:}
        out=$(capture "$lang" "$name" "$args")
        file="$dir/$name.txt"
        if [ "$record" = 1 ]; then
            printf '%s\n' "$out" >"$file"
        elif ! diff -u "$file" <(printf '%s\n' "$out") >"$work/diff" 2>&1; then
            echo "✗ [$lang] $name 与基线不一致" >&2
            sed 's/^/    /' "$work/diff" >&2
            fail=1
        fi
        checked=$((checked + 1))
    done
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
