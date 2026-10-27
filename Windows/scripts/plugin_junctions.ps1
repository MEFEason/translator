<#
    scripts/plugin_junctions.ps1
    修复 Flutter 构建的插件链接问题（Windows + Android 通用）。

    ## 问题
    Flutter 通过 dart:io 的 Link.createSync 在
    ui/<platform>/flutter/ephemeral/.plugin_symlinks/<name>
    建符号链接。在部分 Windows 环境（非管理员 + 未开开发者模式）下，
    这步会失败或**静默退化成空目录** —— 于是：
      1. Gradle/CMake 报 ".plugin_symlinks/xxx 不存在"
      2. 下一次构建时 Link.existsSync() 对"空目录"返回 false，
         Flutter 再次尝试创建 → PathExistsException（errno 183）

    ## 解决
    用 NTFS **junction**（目录联接）替换符号链接：
    junction 不需要管理员权限，也不需要开发者模式，
    且对 Link.existsSync() 与 Gradle/CMake 都是透明的。

    ## 用法
    powershell -ExecutionPolicy Bypass -File scripts/plugin_junctions.ps1
    powershell -ExecutionPolicy Bypass -File scripts/plugin_junctions.ps1 -Platform android
#>

param(
    # 要处理的平台：all（默认） / windows / android
    [string]$Platform = 'all'
)

$ErrorActionPreference = 'Stop'

# ⚠️ Windows PowerShell 5.1 下 $PSScriptDir 在某些调用方式里为 null，
#    $PSCommandPath 才是可靠的。
#
# 目录推导：本脚本在 <repo>/Windows/scripts/ 下，而 Flutter 工程在
# <repo>/translator_core/ui/ —— 差两层，**不能**简单取 parent。
# 改为从脚本所在目录向上找「含 translator_core/ui/pubspec.yaml」的祖先。
$scriptPath = $PSCommandPath
$dir = Split-Path -Parent $scriptPath
$ui = $null
for ($i = 0; $i -lt 5; $i++) {
    $cand = Join-Path $dir 'translator_core\ui\pubspec.yaml'
    if (Test-Path $cand) {
        $ui = Join-Path $dir 'translator_core\ui'
        break
    }
    $parent = Split-Path -Parent $dir
    if (-not $parent -or $parent -eq $dir) { break }
    $dir = $parent
}
if (-not $ui) {
    throw "找不到 translator_core/ui/pubspec.yaml（从 $scriptPath 向上找了 5 层）"
}
$depsFile = Join-Path $ui '.flutter-plugins-dependencies'

if (-not (Test-Path $depsFile)) {
    throw "找不到 $depsFile —— 请先在 ui/ 下执行 flutter pub get"
}

$deps = Get-Content $depsFile -Raw | ConvertFrom-Json

$targets = @()
if ($Platform -eq 'all' -or $Platform -eq 'windows') { $targets += 'windows' }
if ($Platform -eq 'all' -or $Platform -eq 'android') { $targets += 'android' }

$totalFixed = 0
$totalOk = 0

foreach ($plat in $targets) {
    $plugins = $deps.plugins.$plat
    $symlinkDir = Join-Path $ui "$plat\flutter\ephemeral\.plugin_symlinks"

    Write-Host ""
    Write-Host "=== $plat ==="

    if (-not $plugins -or $plugins.Count -eq 0) {
        Write-Host "[INFO] 该平台没有插件，无需处理。"
        continue
    }

    if (-not (Test-Path $symlinkDir)) {
        New-Item -ItemType Directory -Path $symlinkDir -Force | Out-Null
    }

    Write-Host "[INFO] 需要 $($plugins.Count) 个 $plat 插件联接："

    foreach ($p in $plugins) {
        $name = $p.name
        $target = $p.path
        $link = Join-Path $symlinkDir $name

        if (-not (Test-Path $target)) {
            Write-Host ("  [SKIP] {0,-24} 源目录不存在: {1}" -f $name, $target) -ForegroundColor Yellow
            continue
        }

        # 已存在且是有效联接（目标可达 + 有内容）则跳过
        if (Test-Path $link) {
            $childCount = @(Get-ChildItem $link -Force -ErrorAction SilentlyContinue).Count
            if ($childCount -gt 0) {
                Write-Host ("  [OK]   {0,-24} 已是有效联接（{1} 项）" -f $name, $childCount)
                $totalOk++
                continue
            }
            # 空目录 / 失效链接：清掉重建
            [System.IO.Directory]::Delete($link, $true)
        }

        New-Item -ItemType Junction -Path $link -Target $target | Out-Null
        $n = @(Get-ChildItem $link -Force).Count
        Write-Host ("  [FIX]  {0,-24} -> {1}  ({2} 项)" -f $name, (Split-Path $target -Leaf), $n) -ForegroundColor Green
        $totalFixed++
    }
}

Write-Host ""
Write-Host "[PASS] 插件联接就绪（新建 $totalFixed 个，已就绪 $totalOk 个）"
