# 重建代码签名证书（换证书时运行，罕见）
#
# 全程零弹窗：
#   1. 删除旧证书（My 私钥 + Root 注册表项，均静默）
#   2. 新建证书
#   3. 导出 PFX + 仅公钥 CER
#   4. 程序化生成 Blob → 注册表直写装根（静默，不调证书 API）
#   5. 更新 cert_root.reg + setup.iss 的证书段
#
# 依赖 cert_tools.ps1（同目录）。
param()

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "cert_tools.ps1")

$SCRIPTS = $PSScriptRoot
$PFX = Join-Path $SCRIPTS "codesign.pfx"
$CER = Join-Path $SCRIPTS "codesign.cer"
$REGFILE = Join-Path $SCRIPTS "cert_root.reg"
$ISS = Join-Path (Split-Path -Parent $SCRIPTS) "src\setup.iss"

# ── 1. 清理旧证书（注册表直删 + My 存储，全程静默）────────────────
Write-Output "=== 清理旧证书 ==="
$oldCerts = Get-ChildItem Cert:\CurrentUser\Root -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -like "*Translator*" -or $_.Subject -like "*MEFEason*" }
foreach ($old in $oldCerts) {
    $p = "Software\Microsoft\SystemCertificates\Root\Certificates\$($old.Thumbprint)"
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($p, $false)
    Write-Output "  已删除根证书: $($old.Thumbprint)"
}
$myOld = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -like "*Translator*" -or $_.Subject -like "*MEFEason*" }
foreach ($m in $myOld) {
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store(
        "My", [System.Security.Cryptography.X509Certificates.StoreLocation]::CurrentUser)
    $store.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
    $store.Remove($m)
    $store.Close()
    Write-Output "  已删除私钥证书: $($m.Thumbprint)"
}
if (-not $oldCerts -and -not $myOld) { Write-Output "  （无旧证书）" }

# ── 2. 新建证书 ──
Write-Output ""
Write-Output "=== 新建证书 ==="
$cert = New-SelfSignedCertificate `
    -Type CodeSigningCert `
    -Subject $script:CERT_SUBJECT `
    -KeyUsage DigitalSignature `
    -FriendlyName "Translator Code Signing" `
    -NotAfter (Get-Date).AddYears(5) `
    -CertStoreLocation Cert:\CurrentUser\My `
    -HashAlgorithm SHA256
Write-Output "  Subject    : $($cert.Subject)"
Write-Output "  Thumbprint : $($cert.Thumbprint)"

# ── 3. 导出 PFX + 仅公钥 CER ──
$secure = ConvertTo-SecureString -String $script:PFX_PASSWORD -AsPlainText -Force
Export-PfxCertificate -Cert $cert -FilePath $PFX -Password $secure -Force | Out-Null
[System.IO.File]::WriteAllBytes($CER, $cert.RawData)
Write-Output "  PFX / CER 已导出"

# ── 4. 静默装入根证书（注册表直写）─────────────────────────────
Write-Output ""
Write-Output "=== 装入根证书（静默）==="
if (Install-CertRootSilent $PFX $script:PFX_PASSWORD) {
    Write-Output "  根证书已装入且被系统识别"
} else {
    Write-Error "根证书装入失败"
    exit 1
}

# ── 5. 同步 cert_root.reg + setup.iss ──────────────────────────
Write-Output ""
Write-Output "=== 同步安装器数据 ==="
Export-CertRootRegFile $PFX $script:PFX_PASSWORD $REGFILE
Write-Output "  cert_root.reg 已更新"
if (Update-SetupIssBlob $PFX $script:PFX_PASSWORD $ISS) {
    Write-Output "  setup.iss 已同步（新指纹 + 新 Blob）"
} else {
    Write-Output "  [警告] setup.iss 同步失败，请手动检查"
}

Write-Output ""
Write-Output "=== 完成 ==="
Write-Output "NEW_THUMBPRINT=$($cert.Thumbprint)"
Write-Output "下一步：ISCC src\setup.iss 重编安装器；sign_outputs.ps1 重签产物"
