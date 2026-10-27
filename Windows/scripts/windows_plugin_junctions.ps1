<#
    scripts/windows_plugin_junctions.ps1

    修复 Flutter Windows 构建的插件链接问题。

    ## 问题
    Flutter 通过 dart:io 的 Link.createSync 在
    ui/windows/flutter/ephemeral/.plugin_symlinks/<name>
    建符号链接。在部分 Windows 环境（非管理员 + 未开开发者模式）下，
    这步会**静默退化成空目录** —— 于是：

      1. CMake configure 报  add_subdirectory ... .plugin_symlinks/jni/windows 不存在
      2. 下一次构建时 Link.existsSync() 对"空目录"返回 false，
         Flutter 再次尝试创建 → PathExistsException（errno 183）

    ## 解决
    用 NTFS **junction**（目录联接）替换符号链接：
    junction 不需要管理员权限，也不需要开发者模式，
    且对 Link.existsSync() 与 CMake 都是透明的。

    ## 用法
    powershell -ExecutionPolicy Bypass -File scripts/windows_plugin_junctions.ps1
#>

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$ui = Join-Path $repo 'ui'
$depsFile = Join-Path $ui '.flutter-plugins-dependencies'
$symlinkDir = Join-Path $ui 'windows\flutter\ephemeral\.plugin_symlinks'

if (-not (Test-Path $depsFile)) {
    throw "找不到 $depsFile —— 请先在 ui/ 下执行 flutter pub get"
}

$deps = Get-Content $depsFile -Raw | ConvertFrom-Json
$plugins = $deps.plugins.windows

if (-not $plugins -or $plugins.Count -eq 0) {
    Write-Host "[INFO] 该工程没有 Windows 平台插件，无需处理。"
    exit 0
}

if (-not (Test-Path $symlinkDir)) {
    New-Item -ItemType Directory -Path $symlinkDir -Force | Out-Null
}

Write-Host "[INFO] 需要 $($plugins.Count) 个 Windows 插件联接："

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
        $item = Get-Item $link -Force
        $childCount = @(Get-ChildItem $link -Force -ErrorAction SilentlyContinue).Count
        if ($childCount -gt 0) {
            Write-Host ("  [OK]   {0,-24} 已是有效联接（{1} 项）" -f $name, $childCount)
            continue
        }
        # 空目录 / 失效链接：清掉重建
        [System.IO.Directory]::Delete($link, $true)
    }

    New-Item -ItemType Junction -Path $link -Target $target | Out-Null
    $n = @(Get-ChildItem $link -Force).Count
    Write-Host ("  [FIX]  {0,-24} -> {1}  ({2} 项)" -f $name, (Split-Path $target -Leaf), $n) -ForegroundColor Green
}

Write-Host ""
Write-Host "[PASS] 插件联接就绪，可执行 flutter build windows"