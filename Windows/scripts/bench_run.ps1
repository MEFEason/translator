# 占用基准测试：启动 selftest 并按 500ms 采样其 CPU / 内存，输出统计。
#
# 用法（在 translator_core 目录下）：
#   powershell -File scripts/bench_run.ps1 -Label "T2_low" -ExtraArgsStr "--threads 2 --refine false"
#
# 口径说明：
# - selftest 的引擎线程（ASR/MT/监督）都在同一个进程内，采进程级数据即可。
# - CPU 用「进程总处理器时间增量 / 墙钟增量」，得单核百分比（可 >100）；
#   再除以逻辑核数 = 占全系统百分比（与腾讯会议/Zoom 的"CPU +X%"口径一致）。
#   ⚠️ 不用 $proc.Refresh() 后读 TotalProcessorTime —— 实测它对
#   Start-Process 拿到的对象不刷新（采样恒为 0）。每轮重新 Get-Process。
# - 内存取 WorkingSet64（任务管理器"内存"列口径）。

param(
    [Parameter(Mandatory=$true)][string]$Label,
    # 空格分隔的额外参数（不用数组：PS 5.1 会把 "--xxx" 元素解析成本脚本的命名参数）
    [string]$ExtraArgsStr = "",
    [string]$Exe = "core\target\release\selftest.exe",
    [string]$Wav = "bench_60s.wav",
    [int]$SampleMs = 500
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot\..

$ExtraArgs = @()
if ($ExtraArgsStr.Trim().Length -gt 0) {
    $ExtraArgs = $ExtraArgsStr.Trim() -split '\s+'
}

$cores = [Environment]::ProcessorCount
$baseArgs = @(
    "--asr-sherpa", "--mt-bergamot",
    "--model-root", "models",
    "--wav", $Wav,
    "--realtime", "--bench"
)
$allArgs = $baseArgs + $ExtraArgs

# Start-Process 在环境变量同时存在大小写重复的代理键
# （HTTP_PROXY 与 http_proxy）时会抛 "Item has already been added"。
# selftest 完全离线，代理变量对它毫无意义 —— 启动前清掉。
foreach ($k in @('http_proxy','https_proxy','HTTP_PROXY','HTTPS_PROXY')) {
    if (Test-Path "env:$k") { Remove-Item "env:$k" -ErrorAction SilentlyContinue }
}

Write-Host "=== [$Label] 启动: $Exe $($allArgs -join ' ') ==="

$proc = Start-Process -FilePath $Exe -ArgumentList $allArgs -NoNewWindow -PassThru `
    -RedirectStandardError "bench_$Label.log" -RedirectStandardOutput "bench_$Label.out"
$pid2 = $proc.Id

$samples = @()
# ⚠️ 数据源踩坑记录：
# 1. Get-Process.TotalProcessorTime —— 本环境恒为 0（对象缓存不刷新）
# 2. Get-Counter "\Process(x)\% Processor Time" —— 中文 Windows 上恒为 0
# 最终用 WMI 性能数据 Win32_PerfFormattedData_PerfProc_Process，
# 按 ProcessId 精确匹配（避免实例名 #1/#2 歧义）。
# PercentProcessorTime 口径 = 所有核加总（0..核数×100），与任务管理器一致。
while ($true) {
    Start-Sleep -Milliseconds $SampleMs
    $cim = Get-CimInstance -ClassName Win32_PerfFormattedData_PerfProc_Process `
        -Filter "ProcessId=$pid2" -ErrorAction SilentlyContinue
    if ($null -eq $cim) {
        break  # 进程已退出
    }
    $cpu = [double]$cim.PercentProcessorTime
    $mem = [double]$cim.WorkingSet
    $samples += [pscustomobject]@{
        CorePct = $cpu
        SysPct  = $cpu / $cores * 100.0
        MemMB   = $mem / 1MB
    }
}

$proc.WaitForExit(5000) | Out-Null
try { $exit = $proc.ExitCode } catch { $exit = -1 }
if ($samples.Count -eq 0) {
    Write-Host "[$Label] 无采样（进程太快退出？）exit=$exit"
    exit 1
}

$stat = $samples | Measure-Object -Property CorePct, SysPct, MemMB -Maximum -Average
$peakMem = ($samples | Measure-Object MemMB -Maximum).Maximum
# 稳态 = 去掉前 20% 的预热采样（模型加载期）
$warm = [int]($samples.Count * 0.2)
$steady = @($samples | Select-Object -Skip $warm)
$ss = $steady | Measure-Object -Property CorePct, SysPct, MemMB -Maximum -Average

$result = [pscustomobject]@{
    Label           = $Label
    ExtraArgs       = $ExtraArgsStr
    ExitCode        = $exit
    Samples         = $samples.Count
    Cores           = $cores
    AvgCorePct      = [math]::Round($stat.Average.CorePct, 1)
    PeakCorePct     = [math]::Round($stat.Maximum.CorePct, 1)
    AvgSysPct       = [math]::Round($stat.Average.SysPct, 1)
    PeakSysPct      = [math]::Round($stat.Maximum.SysPct, 1)
    PeakMemMB       = [math]::Round($peakMem, 0)
    SteadyAvgCore   = [math]::Round($ss.Average.CorePct, 1)
    SteadyPeakCore  = [math]::Round($ss.Maximum.CorePct, 1)
    SteadyAvgSys    = [math]::Round($ss.Average.SysPct, 1)
    SteadyAvgMemMB  = [math]::Round($ss.Average.MemMB, 0)
    SteadyPeakMemMB = [math]::Round($ss.Maximum.MemMB, 0)
}
$result | ConvertTo-Json | Tee-Object -FilePath "bench_$Label.json"
Write-Host "=== [$Label] 完成 exit=$exit ==="
