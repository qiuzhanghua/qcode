#!/usr/bin/env pwsh
#
# qcode 发布打包（Windows）：文件名沿用 tdp 插件命名约定
#
#     qcode_<version>_<os>_<arch>[_gnu|_musl|_msvc][.exe]
#
# 用法：
#   .\build-for-release.ps1                                  # 本机目标（rustc 报的 host）
#   .\build-for-release.ps1 -Target aarch64-pc-windows-gnullvm
#   .\build-for-release.ps1 -Target x86_64-pc-windows-msvc -OutDir dist -Archive
#
# 与 bash 版一致的三条约定：
#   * 版本只有一个来源：workspace Cargo.toml 的 [workspace.package] version；
#     本机构建的产物会跑一遍 `--version` 对账，对不上就报错。
#   * 交叉工具链用 cot 已装好的配置（llvm-mingw 的 <triple>-clang + 静态 libunwind），
#     变量只在缺失时补，不覆盖用户设置。
#   * 产物落 dist\，不动 target\ 里的中间产物。

param(
    # rust 目标三元组；默认取 rustc -vV 里的 host（Windows 上通常是 gnullvm 或 msvc）
    [string]$Target = "",
    # 产物目录
    [string]$OutDir = "dist",
    # 额外打 zip（包内目录名与包名一致）
    [switch]$Archive,
    # 额外产出自动更新器消费的裸二进制与 SHA256SUMS
    [switch]$Release
)

$ErrorActionPreference = "Stop"

Set-Location -LiteralPath $PSScriptRoot

function Fail([string]$Message) {
    Write-Host "error: $Message"
    exit 1
}

# ---------------------------------------------------------------- 元数据

# 产物名干 <Pkg> 来自 crate 包名，可执行文件名 <Bin> 来自 [[bin]] ——
# qcode 里两者不同（包 qcode / 命令 qc），不能共用一个变量。
$Pkg = (Select-String -Path crates\qcode\Cargo.toml -Pattern '^name\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches.Groups[1].Value
$Bin = (Select-String -Path crates\qcode\Cargo.toml -Pattern '^name\s*=\s*"([^"]+)"' |
    Select-Object -Skip 1 -First 1).Matches.Groups[1].Value
if (-not $Pkg -or -not $Bin) { Fail "读不出 crates\qcode\Cargo.toml 的包名 / [[bin]] 名" }

# 常驻驱动（ADR-0092）与 `qc` 同属一次发布（ADR-0098 §7）。它是**独立 package**
# （ADR-0092 §2），所以必须单独 `-p` 编出来：`-p qcode` 的普通依赖里没有 qcd
# （只在 [dev-dependencies] 里，那是测试用的）。
#
# 漏掉它的后果不是「定时任务用不了」这么轻：GUI 段头那颗「启动」按钮点下去得到
# `qcd` was not found，而 `find_qcd` 第一步就找「与 qc 同目录的兄弟」。
$QcdPkg = (Select-String -Path crates\qcd\Cargo.toml -Pattern '^name\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches.Groups[1].Value
if (-not $QcdPkg) { Fail "读不出 crates\qcd\Cargo.toml 的包名" }
# crates\qcd 没有 [[bin]] 段（一个 package 一个 main.rs），包名即 exe 名。
$QcdBin = $QcdPkg

$Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches.Groups[1].Value
if (-not $Version) { Fail "读不出 Cargo.toml 的 [workspace.package] version" }

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail "PATH 上没有 cargo（先执行 c 激活 cot）" }
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) { Fail "PATH 上没有 rustc（先执行 c 激活 cot）" }

$Sysroot = (rustc --print sysroot).Trim()
$HostTriple = (rustc -vV | Select-String '^host: ').Line.Substring(6).Trim()

if (-not $Target) { $Target = $HostTriple }

if ($Target -notmatch '-windows-') {
    Fail "本脚本只产 Windows 目标；$Target 请在 WSL / Linux 上用 bash 版 build-for-release"
}

# ---------------------------------------------------------------- 命名

switch -Regex ($Target) {
    '^x86_64-' { $Arch = 'amd64' }
    '^aarch64-' { $Arch = 'arm64' }
    '^i686-' { $Arch = '386' }
    default { Fail "不认识的架构：$Target" }
}

$Suffix = ''
if ($Target -match '-msvc$') { $Suffix = '_msvc' }
$Artifact = "${Pkg}_${Version}_windows_${Arch}${Suffix}"

# ------------------------------------------------- 自动更新的资产名（-Release）
#
# crates\qcode\src\update.rs 只认一个形状：<base>-<version>-<os>-<arch>[.exe]，
# 其中的 os / arch 与 Rust 的 std::env::consts 一致（x86_64 / aarch64，
# 不是 amd64 / arm64）。bash 版是同一份约定的另一半。
#
# base 有两个：`qc`（主程序）与 `qcd`（常驻驱动，ADR-0098 §7）。自更新两个都取，
# 所以两个都得发——只发 qc 的话 `qc update` 之后用户得到「新 qc + 没有 qcd」。

function Get-ReleaseArch([string]$Triple) {
    switch -Regex ($Triple) {
        '^x86_64-' { return 'x86_64' }
        '^aarch64-' { return 'aarch64' }
        '^i686-' { return 'x86' }
        default { Fail "自动更新没有为 $Triple 定义架构名" }
    }
}

function Get-ReleaseAsset([string]$Base, [string]$Triple) {
    "$Base-${Version}-windows-$(Get-ReleaseArch $Triple).exe"
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Publish-ReleaseAsset([string]$Base, [string]$Triple, [string]$Source) {
    $name = Get-ReleaseAsset $Base $Triple
    $dest = Join-Path $OutDir $name
    Copy-Item -LiteralPath $Source -Destination $dest -Force
    $sums = Join-Path $OutDir 'SHA256SUMS'
    $lines = @()
    if (Test-Path $sums) {
        $lines = @(Get-Content -LiteralPath $sums | Where-Object { $_ -notmatch "  $([regex]::Escape($name))$" })
    }
    $lines += "$(Get-Sha256 $dest)  $name"
    # LF，不是 CRLF：Linux 的 `sha256sum -c` 会拒绝带 \r 的行。
    [System.IO.File]::WriteAllText((Resolve-Path $OutDir).Path + '\SHA256SUMS', ($lines -join "`n") + "`n")
    Write-Host "  release: $dest"
}

# ---------------------------------------------------------------- 交叉环境

function Set-TargetEnv([string]$Triple, [string]$Kind, [string]$Value) {
    $name = "CARGO_TARGET_" + ($Triple.ToUpper() -replace '-', '_') + "_$Kind"
    # 不覆盖已有设置：cot 的插件已经配好的就别动
    if (-not (Test-Path "env:$name")) { Set-Item -Path "env:$name" -Value $Value }
}

if ($Target -match '-gnullvm$') {
    $MingwTriple = if ($Target -match '^aarch64-') { 'aarch64-w64-mingw32' } else { 'x86_64-w64-mingw32' }
    if (-not (Get-Command "$MingwTriple-clang" -ErrorAction SilentlyContinue)) {
        if ($env:LLVM_MINGW_DIR -and (Test-Path "$env:LLVM_MINGW_DIR\bin\$MingwTriple-clang.exe")) {
            Set-TargetEnv $Target 'LINKER' "$env:LLVM_MINGW_DIR\bin\$MingwTriple-clang.exe"
        } else {
            Fail "PATH 上没有 $MingwTriple-clang（装 cot 的 llvm-mingw 插件）"
        }
    } else {
        Set-TargetEnv $Target 'LINKER' "$MingwTriple-clang"
    }

    # llvm-mingw 的 libunwind.dll.a 会让产物运行期依赖 libunwind.dll；
    # 静态库在 sysroot 的 libunwind-static\<target>\ 里，插到 -L 最前面。
    $StaticUnwind = Join-Path $Sysroot "libunwind-static\$Target"
    if ((Test-Path (Join-Path $StaticUnwind 'libunwind.a'))) {
        Set-TargetEnv $Target 'RUSTFLAGS' "-Lnative=$StaticUnwind"
    } else {
        Write-Host "  提醒：sysroot 里没有 libunwind-static\$Target\libunwind.a"
        Write-Host "        产物运行期会依赖 libunwind.dll（干净的 Windows 上可能加载失败）"
    }
}

# ---------------------------------------------------------------- 构建

Write-Host "==> $Target"
cargo build --locked --release --target $Target -p $Pkg -p $QcdPkg
if ($LASTEXITCODE -ne 0) { Fail "cargo build 失败（$LASTEXITCODE）" }

$Binary = "target\$Target\release\$Bin.exe"
if (-not (Test-Path $Binary)) { Fail "没有构建出 $Binary" }

$DaemonBinary = "target\$Target\release\$QcdBin.exe"
if (-not (Test-Path $DaemonBinary)) { Fail "没有构建出 $DaemonBinary（定时任务没有它就不会触发）" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$Dest = Join-Path $OutDir "$Artifact.exe"
Copy-Item -LiteralPath $Binary -Destination $Dest -Force

# `qcd` 用发布资产名落在 dist\（与 bash 版同形）。`qc` 那个是历史名
# （qcode_<v>_windows_<arch>.exe），保持不变；新加的这一个按新约定命名。
$DaemonDest = Join-Path $OutDir (Get-ReleaseAsset $QcdBin $Target)
Copy-Item -LiteralPath $DaemonBinary -Destination $DaemonDest -Force

# 本机目标能真跑：拿产物自己报的版本与文件名对账
if ($Target -eq $HostTriple) {
    $Reported = (& $Dest --version | ForEach-Object { $_ -split '\s+' } | Select-Object -Last 1)
    if ($Reported -ne $Version) {
        Fail "产物报的版本是 $Reported，文件名里却是 $Version（Cargo.toml 没 bump？）"
    }
    Write-Host "  version: $Reported"
} else {
    Write-Host "  version: $Version（交叉产物，静态核对）"
}

$Size = [math]::Round((Get-Item $Dest).Length / 1MB, 1)
Write-Host "  dist: $Dest ($Size MB)"
$DaemonSize = [math]::Round((Get-Item $DaemonDest).Length / 1MB, 1)
Write-Host "  dist: $DaemonDest ($DaemonSize MB)"

if ($Release) {
    Publish-ReleaseAsset 'qc' $Target $Dest
    # daemon 单独一条资产：`qc update` 按平台挑名字取它（update.rs 的
    # asset_name_for），取不到就只更新 qc——用户于是得到「新 qc + 没有 qcd」。
    Publish-ReleaseAsset $QcdBin $Target $DaemonDest
}

# 一个产物（ADR-0086）：`gui` 现在是默认 feature，上面那条 `cargo build` 出来的
# 就是带窗口的 `qc.exe`。从前这里再编一次（用当年的 `--features gui`）并另发一份
# `qcode-desktop-*.exe`，用两份产物换一次瘦身；现在换回来（实测 34.8 MB vs
# 16.2 MB，见 ADR-0086 §4）。

if ($Archive) {
    $Stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    $Inner = Join-Path $Stage $Artifact
    New-Item -ItemType Directory -Force -Path $Inner | Out-Null
    # 包内是可执行文件名 qc.exe，与产物名区分
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $Inner "$Bin.exe") -Force
    # `qcd.exe` 必须与 `qc.exe` **同级**：解压后的目录就是 `find_qcd` 要找的布局
    # （它第一步只看兄弟文件）。放进子目录等于把一个缺驱动的包交给用户。
    Copy-Item -LiteralPath $DaemonBinary -Destination (Join-Path $Inner "$QcdBin.exe") -Force
    $Zip = Join-Path $OutDir "$Artifact.zip"
    if (Test-Path $Zip) { Remove-Item $Zip -Force }
    Compress-Archive -Path $Inner -DestinationPath $Zip
    Remove-Item -Recurse -Force $Stage
    Write-Host "  archive: $Zip"
}
