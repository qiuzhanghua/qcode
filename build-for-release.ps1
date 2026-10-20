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
# crates\qcode\src\update.rs 只认一个形状：qc-<version>-<os>-<arch>[.exe]，
# 其中的 os / arch 与 Rust 的 std::env::consts 一致（x86_64 / aarch64，
# 不是 amd64 / arm64）。bash 版是同一份约定的另一半。

function Get-ReleaseArch([string]$Triple) {
    switch -Regex ($Triple) {
        '^x86_64-' { return 'x86_64' }
        '^aarch64-' { return 'aarch64' }
        '^i686-' { return 'x86' }
        default { Fail "自动更新没有为 $Triple 定义架构名" }
    }
}

function Get-ReleaseAsset([string]$Triple) {
    "qc-${Version}-windows-$(Get-ReleaseArch $Triple).exe"
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Publish-ReleaseAsset([string]$Triple, [string]$Source) {
    $name = Get-ReleaseAsset $Triple
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
cargo build --locked --release --target $Target -p $Pkg
if ($LASTEXITCODE -ne 0) { Fail "cargo build 失败（$LASTEXITCODE）" }

$Binary = "target\$Target\release\$Bin.exe"
if (-not (Test-Path $Binary)) { Fail "没有构建出 $Binary" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$Dest = Join-Path $OutDir "$Artifact.exe"
Copy-Item -LiteralPath $Binary -Destination $Dest -Force

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

if ($Release) {
    Publish-ReleaseAsset $Target $Dest
}

# 桌面版：同一份代码 + gui 特性。单独命名，避免有人下了 47 MB 才发现只有 CLI。
$DesktopName = "qcode-desktop-${Version}-windows-$(Get-ReleaseArch $Triple).exe"
cargo build --locked --release --target $Target -p $Pkg --features gui
if ($LASTEXITCODE -ne 0) { Fail "cargo build --features gui 失败（$LASTEXITCODE）" }
$DesktopBinary = "target\$Target\release\$Bin.exe"
if (-not (Test-Path $DesktopBinary)) { Fail "没有构建出 $DesktopBinary（gui 特性）" }
$DesktopDest = Join-Path $OutDir $DesktopName
Copy-Item -LiteralPath $DesktopBinary -Destination $DesktopDest -Force
$DesktopSize = [math]::Round((Get-Item $DesktopDest).Length / 1MB, 1)
Write-Host "  dist: $DesktopDest ($DesktopSize MB, with gui)"
if ($Release) {
    Publish-ReleaseAsset $Target $DesktopDest
}

if ($Archive) {
    $Stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    $Inner = Join-Path $Stage $Artifact
    New-Item -ItemType Directory -Force -Path $Inner | Out-Null
    # 包内是可执行文件名 qc.exe，与产物名区分
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $Inner "$Bin.exe") -Force
    $Zip = Join-Path $OutDir "$Artifact.zip"
    if (Test-Path $Zip) { Remove-Item $Zip -Force }
    Compress-Archive -Path $Inner -DestinationPath $Zip
    Remove-Item -Recurse -Force $Stage
    Write-Host "  archive: $Zip"
}
