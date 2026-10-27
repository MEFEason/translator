# 把 sherpa-onnx 的运行时 DLL 复制到目标目录。
#
# 为什么需要这个脚本：sherpa-onnx-sys crate 的 build script 在 shared
# 模式下会自动把 DLL 复制到 target/<profile>/，但该 build script
# （无签名的 build-script-build.exe）在本机复制 DLL 会被安全软件静默
# 拦截（os error 5）。因此改为显式复制：
#   - SHERPA_ONNX_LIB_DIR 指向 link-only/（只有 .lib，无 DLL）
#     → crate 的 build script 只发链接指令，不碰 DLL
#   - DLL 的投放由本脚本完成（PowerShell 进程，不被拦截）
#
# 用法：
#   powershell -File copy_sherpa_dlls.ps1 [-DestDir <目录>]
#   不带参数时复制到 core/target/debug/（开发运行用）
param(
    [string]$DestDir
)

$ErrorActionPreference = "Stop"

$REPO = Split-Path -Parent $PSScriptRoot                    # translator_core/
$CORE = Join-Path $REPO "core"
$LIBDIR = Join-Path $REPO "third_party\sherpa-prebuilt\sherpa-onnx-v1.13.6-win-x64-shared-MT-Release-lib\lib"

if (-not $DestDir) { $DestDir = Join-Path $CORE "target\debug" }
if (-not (Test-Path $LIBDIR)) {
    Write-Error "找不到 sherpa 库目录: $LIBDIR`n先下载并解压 shared-MT 预编译包。"
    exit 1
}

$dlls = Get-ChildItem $LIBDIR -Filter "*.dll"
if (-not $dlls) { Write-Error "$LIBDIR 下没有 DLL"; exit 1 }

New-Item -ItemType Directory -Path $DestDir -Force | Out-Null
foreach ($d in $dlls) {
    Copy-Item $d.FullName (Join-Path $DestDir $d.Name) -Force
    Write-Output "  已复制 $($d.Name)"
}
Write-Output "sherpa DLL 已就位: $DestDir"
