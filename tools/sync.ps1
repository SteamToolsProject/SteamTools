# SteamTools 本地同步脚本 (历史重写后 / 常规同步)
#
# 用法:
#   pwsh tools/sync.ps1        # fetch + 把 dev/master 对齐 origin (强制)
#   pwsh tools/sync.ps1 -DryRun
#
# 注意: 用 reset --hard, 本地未提交/未推送的改动会被丢弃; 先 commit 或 stash。

param(
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

function Log([string]$msg) { Write-Host "[sync] $msg" -ForegroundColor Cyan }

function Reset-Branch([string]$name) {
    Log "同步 $name -> origin/$name"
    if ($DryRun) { return }
    git checkout $name 2>$null
    git fetch origin 2>$null
    git reset --hard "origin/$name"
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot
try {
    $dirty = git status --porcelain
    if ($dirty -and -not $DryRun) {
        Write-Host "工作区有未提交改动, 将被丢弃:" -ForegroundColor Yellow
        $dirty | ForEach-Object { Write-Host "  $_" }
        $answer = Read-Host "确认丢弃并同步? (y/n)"
        if ($answer -notin @('y', 'Y', 'yes')) { throw "已取消" }
    }
    Reset-Branch 'dev'
    Reset-Branch 'master'
    Log "完成: 当前分支 $(git branch --show-current)"
}
finally {
    Pop-Location
}
