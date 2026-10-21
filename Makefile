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
#
# Windows 上用 build-for-release.ps1（见脚本头部）。

SHELL := /bin/bash

# `make` 默认打本机平台；`make all` 才打全矩阵
.DEFAULT_GOAL := native

# 友好名与 build-for-release 的参数一一对应
PLATFORMS := linux-amd64 linux-arm64 linux-musl-amd64 linux-musl-arm64 \
             windows-amd64 windows-arm64 darwin-amd64 darwin-arm64

.PHONY: all native list check perf release archive clean gui app app-debug $(PLATFORMS)

all:
	./build-for-release all

native:
	./build-for-release

list:
	./build-for-release --list

check:
	./build-for-release --check

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

# Build a GUI-enabled qc without letting the next plain `cargo build` /
# `make check` overwrite it. The normal target/debug/qc is the lean binary.
gui:
	CARGO_HOME=$(CURDIR)/.cargo-home cargo build -p qcode --features gui
	cp target/debug/qc target/debug/qc-gui
	@if [ "$$(uname)" = Darwin ]; then \
		codesign --force --sign - target/debug/qc-gui >/dev/null; \
	fi
	@echo "GUI binary: ./target/debug/qc-gui gui"

# Unsigned local macOS app (ad-hoc signed, no Apple account required).
app:
	./scripts/build-macos-app.sh

app-debug:
	./scripts/build-macos-app.sh --debug
