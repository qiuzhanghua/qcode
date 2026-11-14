# 发布构建的快捷入口；真正的逻辑都在 build-for-release（bash）里。
#
#   make                 # 本机平台
#   make all             # 本机能产出的全部平台（Linux 宿主：linux gnu/musl + windows gnullvm）
#   make list            # 矩阵与就绪状态
#   make check           # 发布门禁：fmt / clippy / test / cargo-deny
#   make perf            # 10 万事件会话性能基线（不进入默认 CI）
#   make release         # 门禁 + 全平台归档
#   make linux-musl-amd64 windows-arm64
#   make archive         # 全部平台 + tar.gz/zip
#   make linux-container # 非 Linux 宿主上用 podman 编 linux-amd64 + linux-arm64
#
# Windows 上用 build-for-release.ps1（见脚本头部）。

SHELL := /bin/bash

# `make` 默认打本机平台；`make all` 才打全矩阵
.DEFAULT_GOAL := native

# 友好名与 build-for-release 的参数一一对应
PLATFORMS := linux-amd64 linux-arm64 linux-musl-amd64 linux-musl-arm64 \
             windows-amd64 windows-arm64 darwin-amd64 darwin-arm64

.PHONY: all native list check check-platforms perf release archive clean gui app app-debug linux-container linux-container-archive $(PLATFORMS)

all:
	./build-for-release all

native:
	./build-for-release

list:
	./build-for-release --list

check:
	./build-for-release --check

# Type-check the code this host cannot compile: the Windows and macOS backends
# behind `#[cfg(...)]`. Only needs the target's std (`rustup target add ...`),
# no linker, so it runs in seconds. See scripts/ci-cross-check.sh for why a
# green `cargo test --workspace` on Linux does not mean the tree compiles.
check-platforms:
	./scripts/ci-cross-check.sh

perf:
	./scripts/perf-session.sh

release:
	./build-for-release --check
	./build-for-release --archive all

archive:
	./build-for-release --archive all

clean:
	rm -rf dist

$(PLATFORMS):
	./build-for-release $@

# 曾是「编一份带 gui 的、拷成 qc-gui」的目标。ADR-0086 之后默认构建就是带窗口的
# `qc`，没有「瘦身版」可以对照，也没有 `qc-gui` 这个文件，所以它退化成了一次普通
# 构建——保留名字只是为了让习惯敲 `make gui` 的人不撞空，输出会说明这一点。
#
# 这里曾经无条件 `CARGO_HOME=$(CURDIR)/.cargo-home`，会覆盖调用者已经设好的那份。
# 那既是全仓唯一一处「无条件覆盖」（其余脚本都是「未设置才回退」），也踩中了同一个坑：
# 仓库内的 .cargo-home 通常没有 config.toml 的换源配置。现在什么都不指定，交给环境。
gui:
	cargo build -p qcode
	@echo "one binary now (ADR-0086): ./target/debug/qc — TUI in a terminal, window from an icon"

# Unsigned local macOS app (ad-hoc signed, no Apple account required).
app:
	./scripts/build-macos-app.sh

app-debug:
	./scripts/build-macos-app.sh --debug

# 非 Linux 宿主上编 Linux 产物：走 podman 容器（scripts/Containerfile.linux-builder）。
# `make all` / `make release` 里的 Linux 目标会自动委派到这里，这两个目标只是给
# 「只想编 Linux」的场景一个短入口。
linux-container:
	./scripts/build-linux-in-container.sh

linux-container-archive:
	./scripts/build-linux-in-container.sh --archive
