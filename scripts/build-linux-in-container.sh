#!/usr/bin/env bash
#
# 在 Linux 容器里编 Linux 产物（macOS / Windows 宿主的唯一可行路径）。
#
#   ./scripts/build-linux-in-container.sh                    # linux-amd64 + linux-arm64
#   ./scripts/build-linux-in-container.sh linux-amd64        # 单个目标
#   ./scripts/build-linux-in-container.sh --archive          # 额外打 tar.gz
#   ./scripts/build-linux-in-container.sh --rebuild-image    # 强制重建镜像
#
# 网络慢/拉不动时的两个开关（都不写死默认值）：
#   QCODE_APT_MIRROR=mirrors.aliyun.com          # apt 源
#   QCODE_RUSTUP_DIST_SERVER=https://rsproxy.cn  # rustup 的 std 源
# 宿主代理指向 127.0.0.1 时脚本会自动禁用 build 的代理透传（那个端口在 VM 里不存在）。
#
# 为什么要容器：ADR-0086 之后每个 Linux 产物都要链 gpui，而 gpui 在 Linux 上硬链接
# `-lxcb -lxkbcommon -lxkbcommon-x11`、fontconfig 走 pkg-config。macOS 上既没有这些
# `.so`，也没有目标平台的 `.pc`。硬扛的话要自建一整套 Linux sysroot；容器里这些本来
# 就是发行版的一部分，代价只是镜像。
#
# 镜像定义在 scripts/Containerfile.linux-builder：原生 arm64 依赖 + amd64 multiarch。
# 两个架构都**不靠模拟**——amd64 是交叉编译（rustc 交叉 + x86_64-linux-gnu-gcc），
# 不是在 arm64 VM 里跑 x86_64 镜像。
#
# 产物照常落 dist/，命名与宿主机上跑 build-for-release 一致。arm64 目标在容器里是
# 原生架构，所以 build-for-release 的「跑一遍 --version 对账」对它照旧生效；
# amd64 是交叉产物，只能静态核对（那条本来就只对 HOST_TRIPLE 生效）。
#
# musl 目标不在范围内：容器里没有 musl 交叉 gcc，而 CI 的 release.yml 也只发 gnu
# 两个目标（linux-amd64 / linux-arm64）。需要 musl 就上真 Linux 或补 musl 交叉工具链。

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

IMAGE="${QCODE_LINUX_IMAGE:-qcode-linux-builder:bookworm}"
CONTAINERFILE="$ROOT/scripts/Containerfile.linux-builder"
REBUILD_IMAGE=0

TARGETS=()
ARCHIVE=""
RELEASE=""
for arg in "$@"; do
    case "$arg" in
        --rebuild-image) REBUILD_IMAGE=1 ;;
        --archive) ARCHIVE="--archive" ;;
        --release) RELEASE="--release" ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) echo "error: 不认识的参数：$arg" >&2; exit 2 ;;
        *) TARGETS+=("$arg") ;;
    esac
done
if [ ${#TARGETS[@]} -eq 0 ]; then
    TARGETS=(linux-amd64 linux-arm64)
fi

command -v podman >/dev/null 2>&1 || {
    echo "error: 需要 podman（brew install podman）" >&2
    exit 1
}

# 宿主代理是不是指向本机（127.0.0.1 / localhost）？是的话不能透传进 build 容器——
# 那是宿主上的端口，VM 里不存在。多见于沙箱/本地加速代理。
host_proxy_is_loopback() {
    local v
    for v in "${HTTPS_PROXY:-}" "${https_proxy:-}" "${HTTP_PROXY:-}" "${http_proxy:-}" "${ALL_PROXY:-}" "${all_proxy:-}"; do
        # 注意 user:pass@ 前缀：`http://qcode:token@127.0.0.1:59879` 里 host 不在 `//`
        # 之后，所以中间那段要留 `*`，否则这条检测永远是假的（实测踩过）。
        case "$v" in
            *//*127.0.0.1:*|*//*localhost:*) return 0 ;;
        esac
    done
    return 1
}

# machine 是 macOS 上的 Linux VM。没起就起，已起不动它。
if ! podman info >/dev/null 2>&1; then
    echo "==> 启动 podman machine"
    podman machine start
fi

if [ "$REBUILD_IMAGE" = 1 ] || ! podman image exists "$IMAGE" 2>/dev/null; then
    echo "==> 构建镜像 $IMAGE"
    BUILD_ARGS=()
    # 官方 apt 源在部分网络下拉不动，允许换源（不写死默认值，见 Containerfile）
    [ -n "${QCODE_APT_MIRROR:-}" ] && BUILD_ARGS+=(--build-arg "APT_MIRROR=$QCODE_APT_MIRROR")
    if [ -n "${QCODE_RUSTUP_DIST_SERVER:-}" ]; then
        BUILD_ARGS+=(--build-arg "RUSTUP_DIST_SERVER=$QCODE_RUSTUP_DIST_SERVER")
        BUILD_ARGS+=(--build-arg "RUSTUP_UPDATE_ROOT=${QCODE_RUSTUP_UPDATE_ROOT:-$QCODE_RUSTUP_DIST_SERVER/rustup}")
    fi
    # 宿主代理指向 127.0.0.1 时不能透传进 build：那是一个「在宿主上」的端口，VM 里
    # 不存在，rustup 会报 tunnel error 而看不出原因。detect 到就关掉透传。
    if host_proxy_is_loopback; then
        BUILD_ARGS+=(--http-proxy=false)
    fi
    podman build -t "$IMAGE" -f "$CONTAINERFILE" "${BUILD_ARGS[@]}" "$ROOT"
fi

VM_ARCH=$(podman info --format '{{.Host.Arch}}')
echo "==> podman VM: $VM_ARCH, 目标: ${TARGETS[*]}"

# 宿主 CARGO_HOME 挂进容器，复用已下载的 registry（含 config.toml 里的换源配置）。
# rootless podman 把容器内 root 映射到宿主用户，所以写回来的文件属主还是你。
HOST_CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
if [ ! -d "$HOST_CARGO_HOME" ]; then
    echo "error: 宿主 CARGO_HOME 不存在：$HOST_CARGO_HOME" >&2
    exit 1
fi

# 仓库按**同样的绝对路径**挂进容器：target/ 的指纹里带着路径，换路径等于全部重编。
run_in_container() {
    podman run --rm \
        --userns=keep-id \
        -v "$ROOT:$ROOT" \
        -v "$HOST_CARGO_HOME:/cargo-home" \
        -w "$ROOT" \
        -e CARGO_HOME=/cargo-home \
        -e HOME=/tmp \
        "$IMAGE" \
        bash -c "$1"
}

# 容器内要跑的 build-for-release 调用。--archive 由外层透传。
inner="./build-for-release"
[ -n "$ARCHIVE" ] && inner="$inner $ARCHIVE"
[ -n "$RELEASE" ] && inner="$inner $RELEASE"

for target in "${TARGETS[@]}"; do
    case "$target" in
        linux-amd64|x86_64-unknown-linux-gnu)
            triple=x86_64-unknown-linux-gnu
            # multiarch 交叉：让 pkg-config 只看 amd64 的 .pc，C 编译器用交叉 gcc。
            # build-for-release 只负责 CC/LINKER，这里补 pkg-config 那半套。
            #
            # 别用 `'...'"$var"'...'` 这种拼接：末尾那个引号会把下一行的 `;;` 吞进
            # 字符串，于是下面那个分支根本不存在，两个目标都掉进 `*)`——而报错信息
            # 还写着「收到 linux-arm64」，看着像参数传错了。
            script="export PKG_CONFIG_ALLOW_CROSS=1
                    export PKG_CONFIG_LIBDIR=/usr/lib/x86_64-linux-gnu/pkgconfig
                    export PKG_CONFIG_SYSROOT_DIR=/
                    $inner $triple"
            ;;
        linux-arm64|aarch64-unknown-linux-gnu)
            triple=aarch64-unknown-linux-gnu
            # 容器自身架构，什么都不用改
            script="$inner $triple"
            ;;
        *)
            echo "error: 容器路径只支持 linux-amd64 / linux-arm64（收到 ${target}）" >&2
            exit 2
            ;;
    esac

    echo "==> 容器内构建 $triple"
    run_in_container "$script"
done

echo
echo "产物："
ls -lh "$ROOT"/dist/*linux* 2>/dev/null | awk '{print "  " $9 "  " $5}' || true
