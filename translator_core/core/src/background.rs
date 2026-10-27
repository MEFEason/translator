//! 后台资源占用探测：按**实际 CPU / 内存占用率**判定该用哪档。
//!
//! 背景：翻译器要采集系统内放音频并跑 ASR+MT 推理，必然吃 CPU。用户
//! 机器上常同时跑着视频会议、音乐播放器、浏览器等对延迟敏感的软件。
//! 若翻译器把 CPU 吃满，这些软件就会卡。
//!
//! 判定依据（按优先级）：
//! 1. **系统整体 CPU 使用率** —— 最真实的负载信号
//! 2. **系统整体内存使用率** —— 内存吃紧会触发换页，间接拖慢推理
//! 3. **占用大户 Top 进程** —— 告诉用户「是谁在占」，便于自己判断
//! 4. 软件类别（会议/播放）—— 只作**微调**，不作为主判据
//!
//! 设计约束：
//! - 只读系统计数器，不注入、不 hook、不改任何进程状态
//! - 只做**建议**，绝不自动改用户已选档位
//! - 任何探测失败都退化为「均衡档」，绝不阻断主流程

use std::collections::BTreeMap;

/// 系统整体负载快照。
#[derive(Debug, Clone, Default)]
pub struct SystemLoad {
    /// 全局 CPU 使用率（0-100）
    pub cpu_percent: f64,
    /// 已用内存占比（0-100）
    pub mem_percent: f64,
    /// 物理内存总量（字节）
    pub mem_total: u64,
    /// 可用物理内存（字节）
    pub mem_available: u64,
    /// CPU 占用最高的进程（按 CPU 百分比降序，最多 5 个）
    pub top_processes: Vec<ProcessLoad>,
    /// 检测到的实时通信类软件（辅助信号）
    pub latency_sensitive: Vec<String>,
    /// 检测到的音频播放类软件（辅助信号）
    pub audio_playback: Vec<String>,
}

/// 单个进程的负载。
#[derive(Debug, Clone)]
pub struct ProcessLoad {
    pub name: String,
    /// 占**全局** CPU 的百分比（单核 100% = 全局 100%/核数）
    pub cpu_percent: f64,
    /// 占用内存（字节）
    pub mem_bytes: u64,
}

/// 实时通信类软件（对延迟极敏感）。
const LATENCY_SENSITIVE: &[&str] = &[
    "zoom", "teams", "msteams", "googletalk", "discord", "slack", "wework",
    "feishu", "lark", "voovmeeting", "tencentmeeting", "voomeeting", "meeting",
];

/// 音频播放类软件（与系统内放采集互相干扰）。
const AUDIO_PLAYBACK: &[&str] = &[
    "spotify", "cloudmusic", "neteasemusic", "qqmusic", "kugou", "kuwo",
    "ximalaya", "potplayer", "vlc", "foobar2000", "aimp", "musicbee", "itunes",
];

impl SystemLoad {
    /// 负载是否偏高。
    pub fn is_busy(&self) -> bool {
        self.cpu_percent >= 45.0 || self.mem_percent >= 80.0
    }

    /// 推荐的资源档位键（`light` / `balanced` / `full`）。
    ///
    /// 主判据是资源占用率；软件类别只在**临界区间**做微调。
    ///
    /// ⚠️ 2026-10-25 产品定位纠正（用户明确指出旧逻辑是错的）：
    /// 本软件的**主要使用场景就是视频会议** —— 实时通信软件是**被翻译的
    /// 对象**，字幕跟手才是核心价值，不是要避让的竞争对手。
    /// 旧逻辑在检测到会议软件时把 `full` 压成 `balanced`（理由"别抢会议的
    /// 时间片"），主次颠倒：开会时恰恰最需要识别准、译文快。
    /// 现在只有 CPU/内存真的紧张时才降档，会议软件不再参与降档。
    pub fn recommended_tier(&self) -> &'static str {
        let by_cpu = if self.cpu_percent >= 50.0 {
            "light"
        } else if self.cpu_percent >= 20.0 {
            "balanced"
        } else {
            "full"
        };
        // 内存比 CPU 更敏感：内存吃紧会触发换页，推理延迟会明显抖动。
        let by_mem = if self.mem_percent >= 80.0 {
            "light"
        } else if self.mem_percent >= 50.0 {
            "balanced"
        } else {
            "full"
        };
        // 取两者中更保守的
        let tier = match (by_cpu, by_mem) {
            ("light", _) | (_, "light") => "light",
            ("balanced", _) | (_, "balanced") => "balanced",
            _ => "full",
        };
        tier
    }

    /// 推荐识别线程数（1-8）—— 结合系统空闲资源与转写负载。
    ///
    /// 2026-10-25 实测基准（scripts/bench_run.py，65s 真实音频，20 核机器，
    /// 稳态 = 去掉模型加载预热的前 20% 采样）：
    ///
    /// | 配置 | CPU（单核%） | 占系统 | 内存 |
    /// |---|---|---|---|
    /// | 2 线程关精修 | 165% | 8.3% | 816 MB |
    /// | 4 线程关精修 | 487% | 24.3% | 1285 MB |
    /// | 2 线程开精修 | 192% | 9.6% | 1113 MB |
    /// | 8 线程开精修 | 1269% | 63.4% | 2071 MB |
    ///
    /// 归因：**每 +1 线程 ≈ +160% 单核（1.6 核）+ ~235 MB 内存**
    /// （ORT arena 随线程数扩张）；精修只花 +26% CPU 与 +300 MB。
    /// 对照大厂：腾讯会议完整模式 CPU +11%（系统口径）——
    /// 我们 2 线程 = 8.3% 已在同档，8 线程 = 63% 明显过配。
    ///
    /// 算法（用上面的成本模型反推）：
    /// 1. 可用核 = 总核 × (1 - CPU% ) × 安全系数（转写中 0.5，空闲 0.7）
    /// 2. 线程 = 可用核 ÷ 1.6（每线程实测成本），夹到 1..8
    /// 3. 内存底线：可用 < 2.5 GB 压到 ≤4，< 1.5 GB 压到 ≤2
    pub fn recommended_threads(&self, running: bool) -> usize {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4) as f64;
        // 转写中要留一半给会议软件本身（它是被翻译的对象，不能被饿着）
        let safety = if running { 0.5 } else { 0.7 };
        let free_cores = cores * (1.0 - self.cpu_percent / 100.0) * safety;
        // 每线程实测成本 ≈ 1.6 核（ORT intra-op + 引擎线程 + 自旋开销）
        let by_cpu = (free_cores / 1.6).floor() as i64;
        // 内存底线：2 线程关精修 ~0.8GB，每加一线程 +~0.24GB，精修 +~0.3GB
        let by_mem = if self.mem_available < 1_500_000_000 {
            2
        } else {
            4
        };
        // **封顶 4**（2026-10-25 实测）：>4 线程质量与速度都无收益
        // （四组配置质量数据完全一致；2 线程实时喂入已跟得上，4 = 一倍余量），
        // 而 8 线程在 20 核空闲机也要 63% CPU / 2GB —— 空转抢功耗、
        // 笔记本发热降频，反成卡顿源。用户要更高可手动拖 Slider。
        by_cpu.min(by_mem).clamp(1, 4) as usize
    }

    /// 给用户看的一句话建议。
    pub fn advice(&self) -> String {
        let tier_name = match self.recommended_tier() {
            "light" => "轻量",
            "balanced" => "均衡",
            _ => "全力",
        };
        let mut s = format!(
            "当前 CPU {:.0}%、内存 {:.0}%，建议使用「{}」档。",
            self.cpu_percent, self.mem_percent, tier_name
        );
        if let Some(p) = self.top_processes.first() {
            s.push_str(&format!("（占用最多：{} {:.0}%）", p.name, p.cpu_percent));
        }
        if !self.latency_sensitive.is_empty() {
            s.push_str(&format!(
                "；检测到实时通信软件（{}）—— 实时字幕场景，「全力」档最合适",
                self.latency_sensitive.join("、")
            ));
        }
        s
    }

    /// 序列化给 FFI 层（UI 展示用）。
    pub fn to_json(&self) -> String {
        let procs = self
            .top_processes
            .iter()
            .map(|p| {
                format!(
                    r#"{{"name":"{}","cpu":{:.1},"mem":{}}}"#,
                    escape_json(&p.name),
                    p.cpu_percent,
                    p.mem_bytes
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let lat = self
            .latency_sensitive
            .iter()
            .map(|s| format!("\"{}\"", escape_json(s)))
            .collect::<Vec<_>>()
            .join(",");
        let aud = self
            .audio_playback
            .iter()
            .map(|s| format!("\"{}\"", escape_json(s)))
            .collect::<Vec<_>>()
            .join(",");
        // 两个推荐线程数：空闲 / 转写中。probe 是无会话调用，不知道
        // running 状态，UI 按当前是否转写中取用对应值。
        let rec_idle = self.recommended_threads(false);
        let rec_running = self.recommended_threads(true);
        format!(
            r#"{{"cpu":{:.1},"mem":{:.1},"mem_total":{},"mem_available":{},"recommended":"{}","rec_threads_idle":{},"rec_threads_running":{},"advice":"{}","top":[{}],"latency_sensitive":[{}],"audio_playback":[{}]}}"#,
            self.cpu_percent,
            self.mem_percent,
            self.mem_total,
            self.mem_available,
            self.recommended_tier(),
            rec_idle,
            rec_running,
            escape_json(&self.advice()),
            procs,
            lat,
            aud
        )
    }
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ══════════════════════════════════════════════════════════════
//  Windows 实现
// ══════════════════════════════════════════════════════════════

#[cfg(windows)]
mod imp {
    use std::collections::BTreeMap;
    use std::ffi::c_void;

    pub const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    pub const INVALID_HANDLE_VALUE: isize = -1;

    #[repr(C)]
    pub struct PROCESSENTRY32W {
        pub dwSize: u32,
        pub cntUsage: u32,
        pub th32ProcessID: u32,
        pub th32DefaultHeapID: usize,
        pub th32ModuleID: u32,
        pub cntThreads: u32,
        pub th32ParentProcessID: u32,
        pub pcPriClassBase: i32,
        pub dwFlags: u32,
        pub szExeFile: [u16; 260],
    }

    #[repr(C)]
    pub struct MEMORYSTATUSEX {
        pub dwLength: u32,
        pub dwMemoryLoad: u32,
        pub ullTotalPhys: u64,
        pub ullAvailPhys: u64,
        pub ullTotalPageFile: u64,
        pub ullAvailPageFile: u64,
        pub ullTotalVirtual: u64,
        pub ullAvailVirtual: u64,
        pub ullAvailExtendedVirtual: u64,
    }

    /// PROCESS_MEMORY_COUNTERS（psapi）
    #[repr(C)]
    pub struct PROCESS_MEMORY_COUNTERS {
        pub cb: u32,
        pub PageFaultCount: u32,
        pub PeakWorkingSetSize: usize,
        pub WorkingSetSize: usize,
        pub QuotaPeakPagedPoolUsage: usize,
        pub QuotaPagedPoolUsage: usize,
        pub QuotaPeakNonPagedPoolUsage: usize,
        pub QuotaNonPagedPoolUsage: usize,
        pub PagefileUsage: usize,
        pub PeakPagefileUsage: usize,
    }

    extern "system" {
        pub fn GetSystemTimes(
            lpIdleTime: *mut u64,
            lpKernelTime: *mut u64,
            lpUserTime: *mut u64,
        ) -> i32;
        pub fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
        pub fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> *mut c_void;
        pub fn Process32FirstW(hSnapshot: *mut c_void, lppe: *mut PROCESSENTRY32W) -> i32;
        pub fn Process32NextW(hSnapshot: *mut c_void, lppe: *mut PROCESSENTRY32W) -> i32;
        pub fn OpenProcess(
            dwDesiredAccess: u32,
            bInheritHandle: i32,
            dwProcessId: u32,
        ) -> *mut c_void;
        pub fn GetProcessTimes(
            hProcess: *mut c_void,
            lpCreationTime: *mut u64,
            lpExitTime: *mut u64,
            lpKernelTime: *mut u64,
            lpUserTime: *mut u64,
        ) -> i32;
        pub fn CloseHandle(hObject: *mut c_void) -> i32;
    }

    // psapi.dll 单独加载：GetProcessMemoryInfo 不在 kernel32 导出里，
    // 静态链接 psapi 会给整个 exe 增加一个依赖，动态加载更干净。
    #[link(name = "psapi")]
    extern "system" {
        pub fn GetProcessMemoryInfo(
            hProcess: *mut c_void,
            ppsmemCounters: *mut PROCESS_MEMORY_COUNTERS,
            cb: u32,
        ) -> i32;
    }

    /// 单次采样的系统时间与进程时间。
    pub struct Snapshot {
        pub idle: u64,
        pub kernel: u64,
        pub user: u64,
        /// pid → (进程名, CPU 累计 100ns 刻度, 工作集字节)
        pub procs: BTreeMap<u32, (String, u64, u64)>,
    }

    pub fn take_snapshot() -> Option<Snapshot> {
        let (mut idle, mut kernel, mut user) = (0u64, 0u64, 0u64);
        unsafe {
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
                return None;
            }
        }

        let mut procs = BTreeMap::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == INVALID_HANDLE_VALUE {
                // 进程枚举失败不影响 CPU/内存统计
                return Some(Snapshot { idle, kernel, user, procs });
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    let pid = entry.th32ProcessID;
                    let name = {
                        let len = entry
                            .szExeFile
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szExeFile.len());
                        String::from_utf16_lossy(&entry.szExeFile[..len])
                    };
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if !h.is_null() {
                        let (mut ct, mut et, mut kt, mut ut) = (0u64, 0u64, 0u64, 0u64);
                        if GetProcessTimes(h, &mut ct, &mut et, &mut kt, &mut ut) != 0 {
                            let cpu_ticks = kt.saturating_add(ut);
                            let work_set = process_work_set(h, pid);
                            procs.insert(pid, (name, cpu_ticks, work_set));
                        }
                        CloseHandle(h);
                    }
                    if Process32NextW(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        Some(Snapshot { idle, kernel, user, procs })
    }

    /// 读进程工作集（占用内存字节数）。
    fn process_work_set(h: *mut c_void, _pid: u32) -> u64 {
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        unsafe {
            if GetProcessMemoryInfo(h, &mut c, c.cb) == 0 {
                return 0;
            }
        }
        c.WorkingSetSize as u64
    }
}

#[cfg(windows)]
fn memory_usage() -> (f64, u64, u64) {
    use imp::*;
    let mut st = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    unsafe {
        if GlobalMemoryStatusEx(&mut st) == 0 {
            return (0.0, 0, 0);
        }
    }
    let total = st.ullTotalPhys;
    let avail = st.ullAvailPhys;
    let used_pct = if total > 0 {
        ((total.saturating_sub(avail)) as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    (used_pct, total, avail)
}

#[cfg(not(windows))]
fn memory_usage() -> (f64, u64, u64) {
    (0.0, 0, 0)
}

/// 探测当前系统负载。
///
/// 会阻塞约 250 ms（需要两次采样求 CPU 差值）。这是 UI 里
/// 「检测后台占用」按钮的合理代价。
pub fn probe() -> SystemLoad {
    let mut load = SystemLoad::default();

    let (mem_pct, mem_total, mem_avail) = memory_usage();
    load.mem_percent = mem_pct;
    load.mem_total = mem_total;
    load.mem_available = mem_avail;

    #[cfg(windows)]
    {
        use imp::*;
        let Some(s1) = take_snapshot() else {
            return load;
        };
        // 采样间隔：250 ms 足够算出稳定的 CPU 占用
        std::thread::sleep(std::time::Duration::from_millis(250));
        let Some(s2) = take_snapshot() else {
            return load;
        };

        // 全局 CPU：kernel 含 idle
        let total1 = s1.kernel.saturating_add(s1.user);
        let total2 = s2.kernel.saturating_add(s2.user);
        let total_delta = total2.saturating_sub(total1);
        let idle_delta = s2.idle.saturating_sub(s1.idle);
        if total_delta > 0 {
            let busy = total_delta.saturating_sub(idle_delta);
            load.cpu_percent = (busy as f64 / total_delta as f64) * 100.0;
        }

        // 进程级 CPU：各进程 ticks 差 / 全局 ticks 差
        let mut procs: Vec<ProcessLoad> = Vec::new();
        for (pid, (name, ticks1, mem)) in &s2.procs {
            let Some((_, ticks0, _)) = s1.procs.get(pid) else {
                continue;
            };
            let delta = ticks1.saturating_sub(*ticks0);
            if delta == 0 {
                continue;
            }
            let pct = if total_delta > 0 {
                (delta as f64 / total_delta as f64) * 100.0
            } else {
                0.0
            };
            if pct < 0.5 {
                continue; // 过滤噪声
            }
            procs.push(ProcessLoad {
                name: name.clone(),
                cpu_percent: pct,
                mem_bytes: *mem,
            });
        }
        // 同名进程（多实例，如浏览器多标签、WorkBuddy 多进程）合并，
        // 否则 Top 列表会出现好几个同名项，误导用户。
        procs.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut merged: Vec<ProcessLoad> = Vec::new();
        for p in procs {
            match merged.iter_mut().find(|m| m.name == p.name) {
                Some(m) => {
                    m.cpu_percent += p.cpu_percent;
                    m.mem_bytes += p.mem_bytes;
                }
                None => merged.push(p),
            }
        }
        merged.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.truncate(5);
        load.top_processes = merged;

        // 软件类别（仅作微调信号）
        for p in load.top_processes.clone() {
            classify(&p.name, &mut load);
        }
    }

    load
}

fn classify(name: &str, load: &mut SystemLoad) {
    let lower = name.to_ascii_lowercase();
    if LATENCY_SENSITIVE.iter().any(|k| lower.contains(k))
        && !load.latency_sensitive.iter().any(|n| n == &lower)
    {
        load.latency_sensitive.push(lower.clone());
    }
    if AUDIO_PLAYBACK.iter().any(|k| lower.contains(k))
        && !load.audio_playback.iter().any(|n| n == &lower)
    {
        load.audio_playback.push(lower);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_machine_recommends_full() {
        let load = SystemLoad {
            cpu_percent: 5.0,
            mem_percent: 40.0,
            ..Default::default()
        };
        assert_eq!(load.recommended_tier(), "full");
    }

    #[test]
    fn high_cpu_recommends_light() {
        let load = SystemLoad {
            cpu_percent: 70.0,
            mem_percent: 40.0,
            ..Default::default()
        };
        assert_eq!(load.recommended_tier(), "light");
    }

    #[test]
    fn high_mem_recommends_light() {
        let load = SystemLoad {
            cpu_percent: 10.0,
            mem_percent: 92.0,
            ..Default::default()
        };
        assert_eq!(load.recommended_tier(), "light");
    }

    #[test]
    fn borderline_recommends_balanced() {
        let load = SystemLoad {
            cpu_percent: 30.0,
            mem_percent: 50.0,
            ..Default::default()
        };
        assert_eq!(load.recommended_tier(), "balanced");
    }

    #[test]
    fn mid_memory_recommends_balanced() {
        let load = SystemLoad {
            cpu_percent: 5.0,
            mem_percent: 55.0,
            ..Default::default()
        };
        assert_eq!(load.recommended_tier(), "balanced");
    }

    #[test]
    fn meeting_software_does_not_block_full() {
        // 2026-10-25 产品定位纠正：会议软件是本软件的主要服务对象，
        // 不是避让对象。占用不高时即便在开会也应推荐「全力」。
        let load = SystemLoad {
            cpu_percent: 5.0,
            mem_percent: 40.0,
            latency_sensitive: vec!["zoom.exe".into()],
            ..Default::default()
        };
        assert_eq!(load.recommended_tier(), "full");
    }

    #[test]
    fn probe_returns_sane_values() {
        let load = probe();
        assert!(load.cpu_percent >= 0.0 && load.cpu_percent <= 100.0);
        assert!(load.mem_percent >= 0.0 && load.mem_percent <= 100.0);
        // JSON 可解析
        let j = load.to_json();
        assert!(j.starts_with('{') && j.ends_with('}'));
    }
}
