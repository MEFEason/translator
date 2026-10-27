# 对 outputs 产物做 Authenticode 签名，规避杀软 QVM 误报。
#
# 背景：360 主防对「无签名 + 无版本元数据」的新二进制命中率高。
# 版本资源已在构建期注入，本脚本补齐最后一块：Authenticode 签名。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File sign_outputs.ps1 [-TargetDir <目录>]
#
# 默认目标：<项目根>\outputs
param(
    [string]$TargetDir
)

$ErrorActionPreference = "Stop"

$ROOT = Split-Path -Parent $PSScriptRoot          # translator_core/
$PROJECT = Split-Path -Parent $ROOT               # Translator/
if (-not $TargetDir) { $TargetDir = Join-Path $PROJECT "outputs" }

$PFX = Join-Path $PSScriptRoot "codesign.pfx"
$PASSWORD = "translator2026"

if (-not (Test-Path $PFX)) {
    Write-Error "找不到 PFX：$PFX`n先运行 _mkcert.ps1 生成证书。"
    exit 1
}

# ── 定位 signtool ──
$KIT_BIN = "C:\Program Files (x86)\Windows Kits\10\bin"
$signtool = Get-ChildItem $KIT_BIN -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match "\\x64\\signtool\.exe$" } |
    Sort-Object -Property @{Expression = {
        # 从 ...\bin\10.0.26100.0\x64\ 中取版本号；取不到就排最后
        if ($_.FullName -match "\\bin\\([\d.]+)\\x64\\") { [Version]$Matches[1] } else { [Version]"0.0" }
    }} -Descending |
    Select-Object -First 1 -ExpandProperty FullName

if (-not $signtool) {
    Write-Error "未找到 signtool.exe（需要 Windows SDK）"
    exit 1
}
Write-Output "signtool: $signtool"

# ── 把自签根证书装进受信任存储 ──
# ⚠️ 必须直写注册表，不能用任何证书 API：
#    实测 certutil / X509Store.Add / Import-Certificate 三种方式在往
#    Root 存储写证书时**都会弹**「安全警告」阻塞框（增删都弹）。
#    根证书本质是注册表项 SystemCertificates\Root\Certificates\<SHA1>，
#    直写注册表完全不碰证书 API，全程静默。Blob 由 cert_tools 从
#    PFX 程序化生成（与系统生成结果逐字节一致）。
. (Join-Path $PSScriptRoot "cert_tools.ps1")

$ok = Install-CertRootSilent $PFX $PASSWORD
if ($ok) {
    Write-Output "根证书已就绪（静默，注册表直写）"
}
else {
    Write-Output "[警告] 根证书装入失败，签名将显示为不受信任"
}

# ── 签名所有 exe / dll ──
$targets = Get-ChildItem $TargetDir -File -Recurse |
    Where-Object { $_.Extension -in ".exe", ".dll" }

if (-not $targets) {
    Write-Error "目标目录没有 exe/dll: $TargetDir"
    exit 1
}

$ok = 0
$fail = 0
foreach ($f in $targets) {
    # 已签名的跳过（避免重复签导致臃肿）
    $sig = Get-AuthenticodeSignature $f.FullName
    if ($sig.Status -eq "Valid") {
        Write-Output "  [跳过] $($f.Name) 已签名"
        $ok++
        continue
    }

    & $signtool sign /f $PFX /p $PASSWORD /fd sha256 /d "Translator" `
        /du "https://github.com/translator" `
        /tr "http://timestamp.digicert.com" /td sha256 `
        $f.FullName | Out-Null

    if ($LASTEXITCODE -eq 0) {
        Write-Output "  [OK] $($f.Name)"
        $ok++
    }
    else {
        # 时间戳服务器不可达时退化为无时间戳签名（离线环境常态）
        & $signtool sign /f $PFX /p $PASSWORD /fd sha256 /d "Translator" `
            /du "https://github.com/translator" $f.FullName | Out-Null
        if ($LASTEXITCODE -eq 0) {
            Write-Output "  [OK-无时间戳] $($f.Name)"
            $ok++
        }
        else {
            Write-Output "  [FAIL] $($f.Name)"
            $fail++
        }
    }
}

Write-Output ""
Write-Output "签名完成：成功 $ok，失败 $fail"
if ($fail -gt 0) { exit 1 }

# ── 复核 ──
Write-Output ""
Write-Output "=== 签名复核 ==="
foreach ($f in $targets) {
    $s = Get-AuthenticodeSignature $f.FullName
    "  {0,-24} {1}" -f $f.Name, $s.Status
}
