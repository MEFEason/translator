#!/usr/bin/env python3
"""占用基准测试：启动 selftest 并按固定间隔采样其 CPU / 内存，输出统计 JSON。

数据源用 Win32 GetProcessTimes + GetProcessMemoryInfo（ctypes 直调），
绕开 PowerShell 的 Get-Process / Get-Counter / WMI 在本环境的各种不刷新坑。

用法（在 translator_core 目录下）：
  python scripts/bench_run.py --label T2_low --threads 2 --refine false
"""
import argparse
import ctypes
import json
import subprocess
import sys
import time
from ctypes import wintypes

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
psapi = ctypes.WinDLL("psapi", use_last_error=True)


class FILETIME(ctypes.Structure):
    _fields_ = [("dwLowDateTime", wintypes.DWORD), ("dwHighDateTime", wintypes.DWORD)]


class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
    # ⚠️ 完整 10 个字段，64 位 sizeof = 72。
    # 早期漏了 QuotaPagedPoolUsage / QuotaNonPagedPoolUsage 两个非 Peak 字段
    # （写成 56 字节），K32GetProcessMemoryInfo 校验 cb 时报 err=122。
    _fields_ = [
        ("cb", wintypes.DWORD),
        ("PageFaultCount", wintypes.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


# ⚠️ 必须显式声明签名：64 位下不声明 restype 会被截断成 int，
# OpenProcess 返回的伪句柄直接失效（本项目 ctypes 测 DLL 的老坑）。
kernel32.OpenProcess.restype = wintypes.HANDLE
kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
kernel32.GetProcessTimes.restype = wintypes.BOOL
kernel32.GetProcessTimes.argtypes = [
    wintypes.HANDLE,
    ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME),
    ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME),
]
psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
# 第二参必须是 POINTER(PROCESS_MEMORY_COUNTERS)，用 c_void_p 会让
# ctypes 把 byref 对象当整数传，cb 字段错位 → err=122 缓冲区不足。
psapi.GetProcessMemoryInfo.argtypes = [
    wintypes.HANDLE,
    ctypes.POINTER(PROCESS_MEMORY_COUNTERS),
    wintypes.DWORD,
]


def _ft_to_sec(ft: FILETIME) -> float:
    v = (ft.dwHighDateTime << 32) | ft.dwLowDateTime
    return v / 1e7  # 100ns 单位


def cpu_seconds(handle) -> float:
    """进程累计 CPU 时间（user + kernel），单位秒。"""
    creation = FILETIME(); exit_ = FILETIME()
    kernel = FILETIME(); user = FILETIME()
    ok = kernel32.GetProcessTimes(
        handle, ctypes.byref(creation), ctypes.byref(exit_),
        ctypes.byref(kernel), ctypes.byref(user),
    )
    if not ok:
        return 0.0
    return _ft_to_sec(kernel) + _ft_to_sec(user)


def working_set_mb(pid: int) -> float:
    # GetProcessMemoryInfo 需要 PROCESS_QUERY_INFORMATION(0x0400) |
    # PROCESS_VM_READ(0x0010)。用 QUERY_LIMITED(0x1000) 会返回 0。
    h = kernel32.OpenProcess(0x0400 | 0x0010, False, pid)
    if not h:
        return 0.0
    try:
        pmc = PROCESS_MEMORY_COUNTERS()
        pmc.cb = ctypes.sizeof(pmc)
        if psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb):
            return pmc.WorkingSetSize / (1024 * 1024)
        return 0.0
    finally:
        kernel32.CloseHandle(h)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--threads", type=int, default=0)
    ap.add_argument("--refine", default="true")
    ap.add_argument("--wav", default="bench_60s.wav")
    ap.add_argument("--exe", default=r"core\target\release\selftest.exe")
    ap.add_argument("--sample-ms", type=int, default=500)
    args = ap.parse_args()

    cmd = [
        args.exe,
        "--asr-sherpa", "--mt-bergamot",
        "--model-root", "models",
        "--wav", args.wav,
        "--realtime", "--bench",
    ]
    if args.threads > 0:
        cmd += ["--threads", str(args.threads)]
    cmd += ["--refine", args.refine]

    print(f"=== [{args.label}] 启动: {' '.join(cmd)} ===", flush=True)
    with open(f"bench_{args.label}.log", "wb") as logf:
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=logf)

    cores = multiprocessing_cpu_count()
    samples = []
    prev_cpu = None
    prev_t = None
    try:
        while proc.poll() is None:
            time.sleep(args.sample_ms / 1000.0)
            if proc.poll() is not None:
                break
            h = kernel32.OpenProcess(0x1000 | 0x0400, False, proc.pid)
            if not h:
                break
            try:
                cpu = cpu_seconds(h)
            finally:
                kernel32.CloseHandle(h)
            mem = working_set_mb(proc.pid)
            now = time.perf_counter()
            if prev_cpu is not None and now > prev_t:
                # core_pct：单核口径百分比（166 = 1.66 个核），可 >100
                core_pct = (cpu - prev_cpu) / (now - prev_t) * 100.0
                # sys_pct：占全系统百分比 = core_pct / 核数（core_pct 已是
                # 百分比，**不要再乘 100**）
                samples.append((core_pct, core_pct / cores, mem))
            prev_cpu, prev_t = cpu, now
    finally:
        proc.wait()

    if not samples:
        print(f"[{args.label}] 无采样 exit={proc.returncode}")
        sys.exit(1)

    def avg(xs): return sum(xs) / len(xs)
    def mx(xs): return max(xs)
    warm = int(len(samples) * 0.2)
    steady = samples[warm:] or samples
    core = [s[0] for s in samples]
    sysp = [s[1] for s in samples]
    mem = [s[2] for s in samples]
    score = [s[0] for s in steady]
    ssys = [s[1] for s in steady]
    smem = [s[2] for s in steady]

    result = {
        "label": args.label,
        "threads": args.threads,
        "refine": args.refine,
        "wav": args.wav,
        "exit": proc.returncode,
        "samples": len(samples),
        "cores": cores,
        "avg_core_pct": round(avg(core), 1),
        "peak_core_pct": round(mx(core), 1),
        "avg_sys_pct": round(avg(sysp), 1),
        "peak_sys_pct": round(mx(sysp), 1),
        "peak_mem_mb": round(mx(mem), 0),
        "steady_avg_core_pct": round(avg(score), 1),
        "steady_peak_core_pct": round(mx(score), 1),
        "steady_avg_sys_pct": round(avg(ssys), 1),
        "steady_peak_sys_pct": round(mx(ssys), 1),
        "steady_avg_mem_mb": round(avg(smem), 0),
        "steady_peak_mem_mb": round(mx(smem), 0),
    }
    with open(f"bench_{args.label}.json", "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print(json.dumps(result, ensure_ascii=False, indent=2), flush=True)
    print(f"=== [{args.label}] 完成 exit={proc.returncode} ===", flush=True)


def multiprocessing_cpu_count() -> int:
    import os
    return os.cpu_count() or 1


if __name__ == "__main__":
    main()
