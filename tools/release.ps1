# SteamTools 一键发包脚本
#
# 用法:
#   pwsh tools/release.ps1 -Version v0.1.0          # 完整流程
#   pwsh tools/release.ps1 -Version v0.1.0 -DryRun  # 只打印将执行的动作
#   pwsh tools/release.ps1 -Version v0.1.0 -SkipChecks
#
# 流程:
#   1. 校验版本号与工作区状态, 计算上一个 tag
#   2. (可选) 本地门禁: fmt / clippy / 串行测试
#   3. 构建 release 产物并复制到 dist/
#   4. 用 git log 生成更新日志草稿 dist/release-notes.draft.md
#   5. 暂停: 让 AI 基于草稿总结, 更新 CHANGELOG.md 并加入 [vX.Y.Z] 段落
#   6. 确认后: 提交 CHANGELOG, 打 tag, push 到 origin
#   7. 推送 tag 会触发 Release CI (.github/workflows/release.yml),
#      它读取 CHANGELOG 对应段落并创建 GitHub Release

param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [switch]$SkipChecks,

    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

function Log([string]$msg) { Write-Host "[release] $msg" -ForegroundColor Cyan }

function DryLog([string]$msg) {
    if ($DryRun) { Write-Host "[dryrun] $msg" -ForegroundColor Yellow }
}

function Invoke-Step([string]$name, [scriptblock]$body) {
    Log "== $name =="
    if ($DryRun) { return }
    & $body
    if ($LASTEXITCODE -ne 0) { throw "step failed: $name" }
}

# ---------- 1. 校验 ----------
if ($Version -notmatch '^v\d+\.\d+\.\d+$') {
    throw "Version 必须是 vX.Y.Z 格式, 例如 v0.1.0 (收到: $Version)"
}
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw "需要 GitHub CLI (gh)。请先安装: https://cli.github.com/"
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot
try {
    # 计算上一个 tag (无 tag 时为 $null)
    $prevTag = (git describe --tags --abbrev=0 2>$null)
    if (-not $prevTag) {
        Log "未找到已有 tag, 将生成完整更新日志"
    } else {
        Log "上一个 tag: $prevTag"
    }

    # 工作区必须干净, 但允许 CHANGELOG.md 未提交 (发布流程自己提交它)
    $dirty = git status --porcelain | Where-Object { $_ -notmatch '^\?\? .*CHANGELOG\.md$|^ M CHANGELOG\.md$' }
    if ($dirty) {
        Write-Host "工作区有未提交改动, 请先处理:" -ForegroundColor Yellow
        $dirty | ForEach-Object { Write-Host "  $_" }
        throw "工作区不干净, 中止发布"
    }

    # ---------- 2. 门禁 ----------
    if (-not $SkipChecks) {
        Invoke-Step "cargo fmt --check" { cargo fmt --all -- --check }
        Invoke-Step "cargo clippy" { cargo clippy --workspace --all-targets --all-features -- -D warnings }
        Invoke-Step "cargo test (serial)" { cargo test --workspace --all-features --no-fail-fast -- --test-threads=1 }
    } else {
        Log "跳过门禁 (-SkipChecks)"
    }

    # ---------- 3. 构建与打包 ----------
    Invoke-Step "cargo build --release" {
        cargo build -p stt-host -p stt-store-accel -p stt-loader-dwmapi -p stt-loader-xinput --release
    }
    $dist = Join-Path $RepoRoot 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    Copy-Item target/release/SteamTools.dll  $dist -Force
    Copy-Item target/release/dwmapi.dll      $dist -Force
    Copy-Item target/release/xinput1_4.dll   $dist -Force
    Copy-Item LICENSE $dist -Force
    Copy-Item README.md $dist -Force
    Log "产物已复制到 dist/"

    # ---------- 4. 更新日志草稿 ----------
    $draft = Join-Path $dist 'release-notes.draft.md'
    if ($prevTag) {
        git log "$prevTag..HEAD" --pretty=format:"- %h %s" > $draft
    } else {
        git log HEAD --pretty=format:"- %h %s" > $draft
    }
    Log "更新日志草稿: $draft (请让 AI 总结并更新 CHANGELOG.md)"

    # ---------- 5. 人工/AI 总结确认 ----------
    if (-not (Test-Path (Join-Path $RepoRoot 'CHANGELOG.md'))) {
        throw "缺少 CHANGELOG.md。先创建它, 并加入 $Version 段落"
    }
    $hasEntry = Select-String -Path (Join-Path $RepoRoot 'CHANGELOG.md') -Pattern "^## \[$Version\]" -Quiet
    if (-not $hasEntry) {
        Write-Host "CHANGELOG.md 中还没有 [$Version] 段落。请先让 AI 总结草稿并更新 CHANGELOG.md。" -ForegroundColor Yellow
        if (-not $DryRun) {
            $answer = Read-Host "更新好 CHANGELOG.md 后输入 y 继续 (或 n 中止)"
            if ($answer -notin @('y', 'Y', 'yes')) { throw "已中止" }
        }
    }

    # ---------- 6. 提交 + tag + push ----------
    Invoke-Step "commit CHANGELOG.md" {
        git add CHANGELOG.md
        git commit -m "docs: release $Version changelog"
    }
    Invoke-Step "tag $Version" { git tag $Version }
    Invoke-Step "push master" { git push origin master }
    Invoke-Step "push tag $Version" { git push origin $Version }

    Log "完成! Release CI 将自动读取 CHANGELOG [$Version] 段落并创建 GitHub Release。"
    Log "查看: https://github.com/SteamToolsProject/SteamTools/actions"
}
finally {
    Pop-Location
}
