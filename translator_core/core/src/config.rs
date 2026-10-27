//! 应用配置：唯一真相源。
//!
//! 设计约束（DESIGN.md §7.2）：
//! 三端默认值全部在此定义，平台实现**不得**硬编码任何参数，
//! 只允许通过本结构接收覆写。

/// Android 运行时环境。
///
/// 卓易通（HarmonyOS 兼容层）基于华为 iSulad 容器引擎承载 Android APK，
/// 其音频与存储能力与原生 Android 存在实质差异，必须显式区分。
/// 详见 docs/DESIGN.md §2.5
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AndroidRuntime {
    /// 原生 Android 10+ (API 29+)
    #[default]
    Native,
    /// 卓易通容器（HarmonyOS 兼容层，底层 Android 16）
    ZhuoYiTong,
}

impl AndroidRuntime {
    /// 容器内性能弱于原生，线程与模型档位需下调。
    pub fn is_container(&self) -> bool {
        matches!(self, AndroidRuntime::ZhuoYiTong)
    }

    /// 容器内禁用 large ASR 档（200 MB 加载慢且易触发低内存回收）。
    pub fn allow_large_asr(&self) -> bool {
        matches!(self, AndroidRuntime::Native)
    }

    /// 容器内禁止依赖外部存储路径（与宿主存储严格隔离）。
    pub fn use_private_storage_only(&self) -> bool {
        matches!(self, AndroidRuntime::ZhuoYiTong)
    }
}

/// 音频采集能力探测结果。
///
/// 启动期 3 秒自检产出，**必须在 UI 明示**，
/// 禁止静默降级（DESIGN.md §2.5.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureCapability {
    /// 系统内放可用（Windows / macOS / 原生 Android）
    SystemAudio,
    /// 仅麦克风可用，附降级原因
    MicrophoneOnly {
        reason: String,
    },
    /// 完全不可用
    Unavailable {
        reason: String,
    },
}

impl CaptureCapability {
    pub fn is_usable(&self) -> bool {
        !matches!(self, CaptureCapability::Unavailable { .. })
    }

    /// 是否发生了降级（UI 需显示提示条）
    pub fn is_degraded(&self) -> bool {
        matches!(self, CaptureCapability::MicrophoneOnly { .. })
    }

    /// 用户可读的能力描述
    pub fn describe(&self) -> String {
        match self {
            CaptureCapability::SystemAudio => "内放采集：可用".to_string(),
            CaptureCapability::MicrophoneOnly { reason } => {
                format!("内放采集：不可用（{reason}）\n将使用麦克风，外放声音需佩戴耳机。")
            }
            CaptureCapability::Unavailable { reason } => format!("音频采集不可用：{reason}"),
        }
    }
}

/// 性能档位：决定 VAD hangover 与 beam size。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PerformanceProfile {
    /// 300 ms hangover + beam 4：字幕稳定，但略慢
    #[default]
    Stable,
    /// 150 ms hangover + beam 2：更快，代价是断句更激进
    Fast,
}

impl PerformanceProfile {
    pub fn hangover_ms(&self) -> u64 {
        match self {
            // 2026-10-23 用户实测：300ms 太短，教学音频句间一停就切，
            // 字幕碎成「My class」「Twenty eight Boys」一行一个词。
            // 2 秒 = 自然语句内部的停顿不会误断，换句气口才断。
            // 2026-10-24：2000→800ms。之前为治「碎句」提到 2s，代价是每句
            // 说完要再等 2s 静音才成行送 MT —— 用户实测「翻译很慢」。
            // 英语听力句间停顿 >800ms、句内停顿 <300ms，800ms 折中。
            // 2026-10-24：800→600 / 600→450。用户要更跟手。600ms 仍
            // 高于连续演讲的句内停顿（<300ms），不会碎句；比 800 快 200ms。
            PerformanceProfile::Stable => 600,
            PerformanceProfile::Fast => 450,
        }
    }

    pub fn beam_size(&self) -> u32 {
        match self {
            PerformanceProfile::Stable => 4,
            PerformanceProfile::Fast => 2,
        }
    }
}

/// 资源档位：决定**推理线程数**，即翻译器占用多少 CPU。
///
/// 与 [`PerformanceProfile`] 分工：
/// - `PerformanceProfile` 管**断句与 beam size**（字幕观感）
/// - `ResourceTier` 管**线程占用**（对后台软件的影响）
///
/// 设计约束：翻译器**不能**占满全部核心 —— 用户的机器上常同时
/// 跑着视频会议、音乐播放器等对延迟敏感的软件。全速跑 ASR+MT 会
/// 让这些软件卡顿。
/// 2026-10-25：用户实测「提到 4 效果不大」，明确要求全力档提到 6~8
/// → Full 档 ASR 2→8、MT 3→6。Full 档语义本就是「机器空闲、
/// 只要更快更准」，且需用户**显式选择**，不属于默认行为（默认仍 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResourceTier {
    /// 1 线程：同时运行其他重负载（游戏、大型编译）时选它
    Light,
    /// 2 线程：CPU 或内存比较紧张时选它
    Balanced,
    /// 8 线程（识别）/ 6 线程（翻译）：**默认档**。
    ///
    /// 2026-10-25：默认从 Balanced 改为 Full —— 本软件的主要使用场景
    /// 就是视频会议（实时字幕），默认就该给"识别准、译文快"的最佳体验，
    /// 而不是让用户每次手动去开全力。有其他重负载时用户可自行降档。
    #[default]
    Full,
}

impl ResourceTier {
    /// ASR 推理线程数。
    pub fn asr_threads(&self) -> usize {
        match self {
            ResourceTier::Light => 1,
            ResourceTier::Balanced => 2,
            // 2026-10-25 用户明确要求「全力档提到 6~8 线程」
            ResourceTier::Full => 8,
        }
    }

    /// MT 推理线程数。
    ///
    /// 略少于 ASR：翻译是流水线后段，延后一点不影响字幕跟读体验。
    pub fn mt_threads(&self) -> usize {
        match self {
            ResourceTier::Light => 1,
            ResourceTier::Balanced => 2,
            // 2026-10-25 随 Full 档 ASR 一起上调（用户要求 6~8）
            ResourceTier::Full => 6,
        }
    }

    /// 该档位对应的断句/beam 档。
    pub fn performance_profile(&self) -> PerformanceProfile {
        match self {
            // 轻量档要的是「少占资源 + 少延迟」，断句激进一点更实时
            ResourceTier::Light => PerformanceProfile::Fast,
            ResourceTier::Balanced => PerformanceProfile::Stable,
            ResourceTier::Full => PerformanceProfile::Stable,
        }
    }

    /// 精确翻译（beam 4）是否开启。
    pub fn mt_accurate(&self) -> bool {
        matches!(self, ResourceTier::Full)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceTier::Light => "light",
            ResourceTier::Balanced => "balanced",
            ResourceTier::Full => "full",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "light" => ResourceTier::Light,
            "full" => ResourceTier::Full,
            _ => ResourceTier::Balanced,
        }
    }

    /// UI 展示名。
    pub fn display_name(&self) -> &'static str {
        match self {
            ResourceTier::Light => "轻量",
            ResourceTier::Balanced => "均衡",
            ResourceTier::Full => "全力",
        }
    }

    /// UI 展示的适用场景说明。
    ///
    /// ⚠️ **不要在这里写具体线程数** —— 线程数由 `asr_threads()` /
    /// `mt_threads()` 经 FFI 下发，UI 直接渲染真实值（见
    /// `capability_page.dart` 的 subtitle）。文案写死数字会导致
    /// 调线程数后文案不同步（2026-10-25 就发生过：Full 已调到 8，
    /// 文案还写着"占 4 个线程"）。
    pub fn display_hint(&self) -> &'static str {
        match self {
            // 2026-10-25：文案对齐产品定位——视频会议是主要场景，
            // 「全力」是会议场景的推荐档，不再是"机器空闲才用"。
            ResourceTier::Light => "同时运行其他重负载（游戏、大型编译）时选它",
            ResourceTier::Balanced => "CPU 或内存比较紧张时选它",
            ResourceTier::Full => "视频会议等实时字幕场景选它：识别与翻译都快且准",
        }
    }
}

/// 采集音频的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureSource {
    /// 系统内放（默认）
    #[default]
    SystemAudio,
    /// 麦克风（降级路径 / 用户手动选择）
    Microphone,
}

impl AppConfig {
    /// 采集线程数。容器内降低以避免调度争抢。
    ///
    /// ⚠️ 这是**遗留方法**，无调用点。生效的是
    /// `PipelineConfig::asr_threads`（由 `ResourceTier` 统一设定）。
    /// 保留它是为了不破坏潜在的外部引用。
    pub fn asr_threads(&self, cores: usize) -> usize {
        match self.android_runtime {
            AndroidRuntime::Native => (cores / 2).max(1),
            // 容器：cores/3，下限 1
            AndroidRuntime::ZhuoYiTong => (cores / 3).max(1),
        }
    }

    /// 翻译线程数
    pub fn mt_threads(&self, cores: usize) -> usize {
        match self.android_runtime {
            AndroidRuntime::Native => (cores / 2).max(1),
            AndroidRuntime::ZhuoYiTong => (cores / 3).max(1),
        }
    }

    /// 容器内固定 Stable 档：内存波动大，宁慢勿错句。
    pub fn effective_profile(&self) -> PerformanceProfile {
        if self.android_runtime.is_container() {
            PerformanceProfile::Stable
        } else {
            self.performance_profile
        }
    }

    /// 采样率：全链路固定 16 kHz mono f32。
    pub const SAMPLE_RATE: u32 = 16_000;
    /// VAD 帧长：512 样本 = 32 ms @ 16 kHz（Silero 要求）
    pub const VAD_FRAME_SAMPLES: usize = 512;
    /// 单个语音段最大时长，超过强制切分（防内存与延迟失控）
    /// 段长上限（ms）。超过就强制切分。
    /// 2026-10-24：15s→8s。15 秒对字幕太长（用户觉得「翻译很慢、没进字幕」），
    /// 8 秒既保证每行是可读意群，又让译文更快回来。
    pub const MAX_SEGMENT_MS: u64 = 8_000;
    /// 捕获环形缓冲容量（采样数）
    pub const RING_CAPACITY_SAMPLES: usize = 16_000 * 4;
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub android_runtime: AndroidRuntime,
    pub capture_source: CaptureSource,
    pub performance_profile: PerformanceProfile,
    /// 启用说话人分离（当前默认关闭，见 DESIGN.md §11 决策 4）
    pub enable_diarization: bool,
    /// 使用 large ASR 档（容器内强制 false）
    pub use_large_asr: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            android_runtime: AndroidRuntime::default(),
            capture_source: CaptureSource::default(),
            performance_profile: PerformanceProfile::default(),
            enable_diarization: false,
            use_large_asr: false,
        }
    }
}
