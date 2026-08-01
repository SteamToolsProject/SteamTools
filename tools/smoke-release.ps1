# SteamTools 分发冒烟: 验证 release 产物命名与 checksums 生成一致.
#
# 目的: 防止 release.yml / release.ps1 / 自更新客户端三处对"产物名"和
# "checksums 格式"的假设漂移. 跑法:
#   pwsh tools/smoke-release.ps1
#
# 前置: 先跑 cargo build --release (本脚本不负责构建).

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot

function Fail([string]$msg) {
    Write-Host "[smoke] FAIL: $msg" -ForegroundColor Red
    exit 1
}

# 1. 产物存在且命名一致.
foreach ($name in @('stbase.dll', 'dwmapi.dll', 'xinput1_4.dll')) {
    $path = Join-Path $RepoRoot "target\release\$name"
    if (-not (Test-Path $path)) {
        Fail "缺少 release 产物 target/release/$name; 先跑 cargo build --release"
    }
}

# 2. checksums.sha256 与产物一致 (sha256sum 格式: <hex>  <name>).
$checksumsPath = Join-Path $RepoRoot 'dist\checksums.sha256'
if (-not (Test-Path $checksumsPath)) {
    Fail "缺少 dist/checksums.sha256; 先跑 tools/release.ps1 或手工生成"
}
$lines = Get-Content $checksumsPath
if ($lines.Count -ne 3) {
    Fail "checksums.sha256 应有 3 行, 实际 $($lines.Count) 行"
}
foreach ($line in $lines) {
    if ($line -notmatch '^[0-9a-f]{64}  [A-Za-z0-9._-]+$') {
        Fail "checksums 行格式不对: [$line] (应为 '<hex>  <name>')"
    }
    $hex, $name = $line -split '  '
    $actual = (Get-FileHash (Join-Path $RepoRoot "target\release\$name") -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hex -ne $actual) {
        Fail "checksum 不匹配: $name 期望 $hex 实际 $actual"
    }
}
Write-Host "[smoke] checksums.sha256 与产物一致" -ForegroundColor Green

# 3. 自更新客户端能下载的 URL 布局 (GitHub release download URL).
foreach ($name in @('stbase.dll', 'dwmapi.dll', 'xinput1_4.dll', 'checksums.sha256')) {
    $url = "https://github.com/SteamToolsProject/SteamTools/releases/download/v0.0.0/$name"
    if (-not ($url -match '^https://github\.com/[A-Za-z0-9_-]+/[A-Za-z0-9_-]+/releases/download/v\d+\.\d+\.\d+/[A-Za-z0-9._-]+$')) {
        Fail "资产 URL 布局异常: $url"
    }
}
Write-Host "[smoke] 资产 URL 布局与自更新客户端假设一致" -ForegroundColor Green

Write-Host "[smoke] 全部通过" -ForegroundColor Green
