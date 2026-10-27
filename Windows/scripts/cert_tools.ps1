# 证书工具库（供 sign_outputs.ps1 / _renew_cert.ps1 dot-source 引用）
#
# 核心能力：从 PFX **程序化生成**根证书注册表 Blob 并静默装入，
# 完全不调用任何证书 API（实测 certutil / X509Store.Add /
# Import-Certificate 三种方式在写 Root 存储时都会弹「安全警告」
# 阻塞框，增删都弹；只有注册表直写全程静默）。
#
# Blob 结构（1034 字节 = 148 头 + 886 DER），5 条属性记录：
#   每条格式: <id:LE32> <count:LE32=1> <len:LE32> <data>
#   id=0x19  公钥MD5  : MD5(公钥BITSTRING裸值)      16B
#   id=0x0F  签名哈希 : SHA256(TBS含4字节头)         32B
#   id=0x03  SHA1哈希 : SHA1(证书DER)                20B
#   id=0x14  KeyID    : SHA1(公钥BITSTRING裸值)      20B
#   id=0x20  证书本体 : DER

$script:CERT_SUBJECT = "CN=Certificate for Translator by MEFEason, O=MEFEason, C=CN"
$script:PFX_PASSWORD = "translator2026"

# ── 从 PFX 生成根证书注册表 Blob ────────────────────────────────
function New-CertRootBlob([string]$pfxPath, [string]$pfxPassword) {
    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2(
        $pfxPath, $pfxPassword,
        [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::Exportable)

    $der = $cert.RawData
    $pub = $cert.PublicKey.EncodedKeyValue.RawData

    # TBS：DER[4 .. 4+4+tbsLen]；TBS 头紧跟外层 SEQ 头，长度在 der[6..7]
    # ⚠️ 必须先转 [int]：byte 的 -shl 按 byte 宽度溢出（512 会回绕成 0）
    $tbsLen = ([int]$der[6] -shl 8) -bor [int]$der[7]
    $tbs = $der[4..(3 + 4 + $tbsLen)]

    function HashCert([string]$alg, [byte[]]$data) {
        $h = [System.Security.Cryptography.HashAlgorithm]::Create($alg)
        return ,$h.ComputeHash($data)
    }

    $records = @(
        @{ Id = 0x19; Data = (HashCert 'MD5'    $pub) }
        @{ Id = 0x0F; Data = (HashCert 'SHA256' $tbs) }
        @{ Id = 0x03; Data = (HashCert 'SHA1'   $der) }
        @{ Id = 0x14; Data = (HashCert 'SHA1'   $pub) }
        @{ Id = 0x20; Data = $der }
    )

    $ms = New-Object System.IO.MemoryStream
    foreach ($r in $records) {
        $ms.Write([BitConverter]::GetBytes([uint32]$r.Id),  0, 4)
        $ms.Write([BitConverter]::GetBytes([uint32]1),      0, 4)
        $ms.Write([BitConverter]::GetBytes([uint32]$r.Data.Length), 0, 4)
        $ms.Write($r.Data, 0, $r.Data.Length)
    }
    return ,$ms.ToArray()
}

# ── 静默装入根证书（注册表直写）────────────────────────────────
# 返回 $true = 已装入（或原本就在）；$false = 失败
function Install-CertRootSilent([string]$pfxPath, [string]$pfxPassword) {
    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2(
        $pfxPath, $pfxPassword,
        [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::Exportable)

    # 已在存储 → 跳过（幂等）
    $existing = Get-ChildItem Cert:\CurrentUser\Root -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
    if ($existing) { return $true }

    $blob = New-CertRootBlob $pfxPath $pfxPassword
    $regPath = "Software\Microsoft\SystemCertificates\Root\Certificates\$($cert.Thumbprint)"
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($regPath)
    $key.SetValue("Blob", $blob, [Microsoft.Win32.RegistryValueKind]::Binary)
    $key.Close()

    # 验证系统是否识别
    $check = Get-ChildItem Cert:\CurrentUser\Root -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
    return ($null -ne $check)
}

# ── 导出 cert_root.reg（无 PFX 时的手动恢复备用）────────────────
function Export-CertRootRegFile([string]$pfxPath, [string]$pfxPassword, [string]$outFile) {
    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($pfxPath, $pfxPassword)
    $blob = New-CertRootBlob $pfxPath $pfxPassword
    $hex = ($blob | ForEach-Object { $_.ToString("x2") }) -join ","
    $lines = for ($i = 0; $i -lt $hex.Length; $i += 24*3) {
        $chunk = $hex.Substring($i, [Math]::Min(24*3, $hex.Length - $i))
        if ($i + 24*3 -lt $hex.Length) { "$chunk,\" } else { $chunk }
    }
    $content = @"
Windows Registry Editor Version 5.00

[HKEY_CURRENT_USER\Software\Microsoft\SystemCertificates\Root\Certificates\$($cert.Thumbprint)]
"Blob"=hex:$($lines -join "`r`n  ")

"@
    [System.IO.File]::WriteAllText($outFile, $content)
}

# ── 同步 setup.iss 的证书段（指纹 + Blob）───────────────────────
function Update-SetupIssBlob([string]$pfxPath, [string]$pfxPassword, [string]$issPath) {
    if (-not (Test-Path $issPath)) { return $false }
    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($pfxPath, $pfxPassword)
    $blob = New-CertRootBlob $pfxPath $pfxPassword

    $iss = [System.IO.File]::ReadAllText($issPath)
    $marker = 'Root: HKCU; Subkey: "Software\Microsoft\SystemCertificates\Root\Certificates\'
    $idx = $iss.IndexOf($marker)
    if ($idx -lt 0) { return $false }
    $endIdx = $iss.IndexOf("Flags: uninsdeletekey", $idx)
    if ($endIdx -lt 0) { return $false }
    $lineEnd = $iss.IndexOf("`n", $endIdx)

    $innoHex = ($blob | ForEach-Object { $_.ToString("x2") }) -join " "
    $innoLines = for ($i = 0; $i -lt $innoHex.Length; $i += 24*3) {
        $chunk = $innoHex.Substring($i, [Math]::Min(24*3, $innoHex.Length - $i))
        if ($i + 24*3 -lt $innoHex.Length) { "    $chunk\" } else { "    $chunk" }
    }
    $block = @"
Root: HKCU; Subkey: "Software\Microsoft\SystemCertificates\Root\Certificates\$($cert.Thumbprint)"; `
    ValueType: binary; ValueName: "Blob"; ValueData: "$($innoLines -join "`r`n"); `
    Flags: uninsdeletekey createvalueifdoesntexist
"@
    $iss = $iss.Substring(0, $idx) + $block + $iss.Substring($lineEnd + 1)
    [System.IO.File]::WriteAllText($issPath, $iss)
    return $true
}
