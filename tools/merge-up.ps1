# SteamTools 一键合入 dev → master
#
# 用法:
#   pwsh tools/merge-up.ps1          # 完整流程: push dev → 开 PR → 等 CI → 合并 → 同步
#   pwsh tools/merge-up.ps1 -DryRun  # 只打印将执行的动作
#   pwsh tools/merge-up.ps1 -SkipWait# push + 开 PR 后不等待, 手动合并
#
# 前提: 当前在 dev 分支, 工作区干净, gh 已登录。

param(
    [switch]$DryRun,
    [switch]$SkipWait
)

$ErrorActionPreference = 'Stop'

function Log([string]$msg) { Write-Host "[merge-up] $msg" -ForegroundColor Cyan }

function Invoke-Step([string]$name, [scriptblock]$body) {
    Log "== $name =="
    if ($DryRun) { return }
    & $body
    if ($LASTEXITCODE -ne 0) { throw "step failed: $name" }
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot
try {
    # ---------- 1. 前置检查 ----------
    $branch = git branch --show-current
    if ($branch -ne 'dev') { throw "请在 dev 分支执行 (当前: $branch)" }
    $dirty = git status --porcelain
    if ($dirty) {
        Write-Host "工作区有未提交改动, 请先处理:" -ForegroundColor Yellow
        $dirty | ForEach-Object { Write-Host "  $_" }
        throw "工作区不干净, 中止"
    }
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "需要 GitHub CLI (gh)" }

    # ---------- 2. 同步并推送 dev ----------
    Invoke-Step "fetch" { git fetch origin }
    Invoke-Step "push dev" { git push origin dev }

    # ---------- 3. 检查与 master 的差异 ----------
    $ahead = (git rev-list --count origin/master..origin/dev)
    if ($ahead -eq 0) {
        Log "dev 与 master 无差异, 无需合入"
        return
    }
    Log "dev 领先 master $ahead 个提交"

    # ---------- 4. 开 PR ----------
    $title = git log origin/master..origin/dev --pretty=format:"%s" | Select-Object -Last 1
    $prUrl = $null
    Invoke-Step "create PR" {
        $prUrl = gh pr create --base master --head dev --title $title --body "自动合入: dev 领先 master $ahead 个提交`n`n由 tools/merge-up.ps1 发起"
    }
    $prNumber = ($prUrl -split '/')[-1]
    Log "PR: $prUrl"

    # ---------- 5. 等待 CI ----------
    if (-not $SkipWait) {
        Invoke-Step "wait CI" {
            gh pr checks $prNumber --watch --interval 15 | Out-Null
        }
        $conclusion = gh pr checks $prNumber 2>&1 | Select-String -Pattern "^(fail|error)" 
        if ($conclusion) {
            throw "CI 未通过:`n$($conclusion.Line)"
        }
    } else {
        Log "跳过等待 (-SkipWait), 请手动合并: gh pr merge $prNumber --merge"
    }

    # ---------- 6. 合并 ----------
    Invoke-Step "merge PR #$prNumber" { gh pr merge $prNumber --merge }
    Log "已合并: $prUrl"

    # ---------- 7. 本地同步 ----------
    Invoke-Step "sync master" {
        git fetch origin
        git checkout master
        git reset --hard origin/master
        git checkout dev
        git reset --hard origin/dev
    }
    Log "完成! 本地 master/dev 已同步"
}
finally {
    Pop-Location
}
