//! 转写流水线编排层。
//!
//! ## 为什么需要这一层
//!
//! 采集（`platform/`）、断句（`vad/`）、识别（`asr/`）、翻译（`mt/`）、
//! 词库（`glossary/`）、字幕聚合（`subtitle/`）六块零件在写这个模块之前
//! 就已各自完整并有测试，但**没有任何代码把它们串起来** —— 唯一的编排逻辑
//! 曾经只存在于 `tests/pipeline.rs` 这个测试文件里。
//!
//! 本模块就是那条缺失的装配线。
//!
//! ## 三线程模型（§6.2 实时性约束）
//!
//! ```text
//! 采集线程 ──(rtrb 无锁环, 4s)──▶ 引擎线程 ──(有界 32)──▶ MT 线程
//!                                 │
//!                                 └──▶ 事件队列 ──▶ UI 轮询
//! ```
//!
//! 为什么要分开，不能合成一个线程：
//!
//! - **ASR 绝不能放采集线程**：sherpa 的 `accept()` 是同步解码，
//!   3200 样本在 RTF 0.2 的机器上要 40ms。放进去会让 WASAPI 缓冲欠载，
//!   系统直接判为音频故障。
//! - **MT 绝不能在引擎线程内做**：否则译文耗时直接叠加到转写延迟上，
//!   违反「EN 先于 ZH 上屏」（MT 背压，§6.2）。
//!
//! ## 唯一允许出现平台差异的地方
//!
//! 按 DESIGN §7.1，本 crate 内不允许 `#[cfg(target_os)]`，
//! 平台差异全部收敛到 `platform/`。feature 门控同理，全部集中在
//! [`wiring`]，其余文件只依赖 trait。
//!
//! ## 数据一致性：行号（seq）是唯一身份
//!
//! 内存窗口会丢弃旧行，所以**绝不能用数组下标标识一行**。全局单调
//! `line_seq` 永不复用，是内存窗口、转写盘、UI 增量事件三者的共同主键。

pub mod engine;
pub mod wiring;

// LineEvent 定义在本模块（它是契约，不是实现细节），故不在此重导出
pub use engine::{
    PipelineStats, PublishedState, SessionEngine, SessionState, SessionSummary, StartOutcome,
};
pub use wiring::{build_asr_backend, build_mt_backend, BackendChoice};

use std::path::PathBuf;

use crate::config::PerformanceProfile;
use crate::error::Result;

/// 声源类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// 系统内放（对方说的话）
    SystemAudio,
    /// 麦克风（自己说的话）
    Microphone,
}

impl SourceKind {
    /// 默认说话人标签。
    ///
    /// 内放是「对方」，麦克风是「我」—— 这是双语对话字幕的默认语义。
    /// 用户可在 UI 里改成任意名字。
    pub fn default_speaker(&self) -> &'static str {
        match self {
            SourceKind::SystemAudio => "对方",
            SourceKind::Microphone => "我",
        }
    }
}

/// 一个声源的配置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    pub kind: SourceKind,
    /// 说话人标签，出现在字幕行上
    pub speaker: String,
}

impl SourceSpec {
    pub fn new(kind: SourceKind) -> Self {
        Self {
            kind,
            speaker: kind.default_speaker().to_string(),
        }
    }

    pub fn with_speaker(kind: SourceKind, speaker: impl Into<String>) -> Self {
        Self {
            kind,
            speaker: speaker.into(),
        }
    }
}

/// 一次转写会话的配置。
///
/// 刻意**不提供 `from_json`**：JSON 解析属于 FFI 边界的职责
/// （`ffi_session.rs`），核心库不该知道「配置从哪来」。
/// 字段数会随版本增长，用结构体传递比平铺参数更抗 ABI 变动。
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// ASR 模型目录（sherpa zipformer）
    pub asr_model_dir: PathBuf,
    /// MT 权重目录（Bergamot / Firefox Translations）
    pub mt_model_dir: PathBuf,

    /// 词库文件路径。可为空（不加载词库，术语直接交给引擎）。
    pub glossary_paths: Vec<PathBuf>,

    /// 声源列表。空表示单一声源（用 [`Self::default_source`]）。
    pub sources: Vec<SourceSpec>,

    /// 性能档位 → VAD hangover 与 beam size
    pub profile: PerformanceProfile,

    pub asr_threads: usize,
    pub mt_threads: usize,

    /// 资源档位。设置它会**联动**覆盖 `asr_threads` / `mt_threads` /
    /// `profile` / `mt_accurate` 四个字段 —— 用户只需选一个档位，
    /// 不需要理解「线程数」与「断句」是两件事。
    ///
    /// 若上层显式指定了线程数（JSON 里传了 `asr_threads`），
    /// 则以显式值为准，档位只影响断句与精确档。
    pub resource_tier: crate::config::ResourceTier,

    /// true = Accurate 档（beam 4），false = Fast 档（beam 2）
    pub mt_accurate: bool,

    /// 内存里保留多少行。超出丢弃最旧的，转写盘仍保留全部。
    pub memory_window_lines: usize,

    /// 后端来源。`Auto` 按编译 feature 选真实引擎，
    /// `Mock` 强制用内置参考实现（联调与自检用）。
    pub backend: BackendChoice,

    /// 转写盘输出目录。`None` 表示不落盘（纯内存模式）。
    pub transcript_dir: Option<PathBuf>,

    /// 离线精修模型目录（两遍解码**第二遍**，SenseVoice）。
    /// 空 = 不启用第二遍。
    pub offline_model_dir: PathBuf,

    /// 是否启用两遍解码（成行后用离线模型重识别修正文本）。
    pub offline_refine: bool,

    /// MT（翻译）是否**可选**。
    ///
    /// ## 为什么需要这个开关（2026-10-26，Android 阶段1）
    ///
    /// 阶段1 的设备上只装了 ASR 模型（162MB），
    /// Bergamot 权重（50MB）阶段2 才接。此时：
    /// - `validate()` 若强制校验 MT 目录 → **会话创建直接失败**，
    ///   连识别都用不了；
    /// - 但「没装翻译模型」是**正常状态**（不是安装损坏），
    ///   不该当致命错误。
    ///
    /// true = MT 目录缺失时放行（引擎降级为「仅识别」模式，
    /// 由 `mt_ready=0` + UI 明示告知用户）。
    /// false = 严格校验（桌面端默认：MT 目录缺失即报错，
    /// 因为桌面版是「ASR+MT 完整形态」，少一个就是安装不完整）。
    pub mt_optional: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        let tier = crate::config::ResourceTier::default();
        let profile = tier.performance_profile();
        Self {
            asr_model_dir: PathBuf::new(),
            mt_model_dir: PathBuf::new(),
            glossary_paths: Vec::new(),
            sources: vec![SourceSpec::new(SourceKind::SystemAudio)],
            profile,
            asr_threads: tier.asr_threads(),
            mt_threads: tier.mt_threads(),
            resource_tier: tier,
            mt_accurate: tier.mt_accurate(),
            memory_window_lines: 2_000,
            backend: BackendChoice::Auto,
            transcript_dir: None,
            offline_model_dir: PathBuf::new(),
            // 默认开启：模型缺失时 try_new 会安静失败，自动退回单遍流式
            offline_refine: true,
            // 默认严格：MT 目录缺失即报错（桌面端是完整形态）
            mt_optional: false,
        }
    }
}

impl PipelineConfig {
    /// 切换资源档位，联动更新线程数 / 断句 / 精确档。
    ///
    /// 这是 UI 选档位的唯一入口 —— 保证「选了轻量档就真的只用 1 线程」，
    /// 不会出现档位变了但线程数没变的漂移。
    pub fn set_resource_tier(&mut self, tier: crate::config::ResourceTier) {
        self.resource_tier = tier;
        self.asr_threads = tier.asr_threads();
        self.mt_threads = tier.mt_threads();
        self.profile = tier.performance_profile();
        self.mt_accurate = tier.mt_accurate();
    }

    /// 兼容单一声源的旧调用方：没显式给 sources 时用系统内放。
    pub fn default_source(&self) -> SourceSpec {
        self.sources
            .first()
            .cloned()
            .unwrap_or_else(|| SourceSpec::new(SourceKind::SystemAudio))
    }

    /// 用性能档位校准两个引擎配置。
    ///
    /// 之前 `PerformanceProfile` 与 `AsrConfig::with_profile` 都是定义了
    /// 却无人调用 —— 档位选了不起作用，正是因为缺了编排层。
    pub fn asr_config(&self) -> crate::asr::AsrConfig {
        crate::asr::AsrConfig::new(self.asr_model_dir.clone(), self.asr_threads)
            .with_profile(self.profile.beam_size())
    }

    pub fn mt_config(&self) -> crate::mt::MtConfig {
        let mut c = crate::mt::MtConfig::new(self.mt_model_dir.clone(), self.mt_threads);
        // MtConfig 字段是 pub 的，直接设即可 —— 它没有 with_tier，
        // 因为 tier 本来就是配置而不是构建期开关。
        c.tier = if self.mt_accurate {
            crate::mt::QualityTier::Accurate
        } else {
            crate::mt::QualityTier::Fast
        };
        c
    }

    /// 构建离线精修器（两遍解码第二遍）。
    ///
    /// 关闭 / 目录为空 / 模型缺失一律返回 `None` —— 不影响主流程，
    /// 只是退回单遍流式识别。
    pub fn offline_refiner(&self) -> Option<crate::asr::offline::OfflineRefiner> {
        if !self.offline_refine || self.offline_model_dir.as_os_str().is_empty() {
            return None;
        }
        // 2026-10-25 用户明确的两档语义：
        // - 标准档（Stable）= 识别准、翻译准，可以慢一点 → 启用两遍解码精修
        // - 快速档（Fast）  = 快优先、不太精准 → 不精修，纯流式出字
        // 共同底线：都不丢内容（由 engine.rs 的多 Final 跳过逻辑保证）。
        if matches!(self.profile, crate::config::PerformanceProfile::Fast) {
            eprintln!("[offline] 快速档：跳过离线精修（速度优先）");
            return None;
        }
        crate::asr::offline::OfflineRefiner::try_new(&self.offline_model_dir, self.asr_threads)
    }

    /// 校验配置是否可用。**不加载引擎**，只检查目录结构 ——
    /// 启动自检要毫秒级返回，不能等 onnxruntime 初始化完。
    pub fn validate(&self) -> Result<()> {
        crate::modelspec::probe_asr_dir(&self.asr_model_dir)?;
        // MT 可选模式（Android 阶段1）：目录缺失不致命，
        // 引擎降级为「仅识别」模式并由 mt_ready=0 如实上报。
        if !self.mt_optional {
            crate::modelspec::probe_mt_dir(&self.mt_model_dir)?;
        }
        Ok(())
    }
}

/// 运行时可调参数（2026-10-25：转写中动态调整线程/hangover/精修，无需重启）
pub mod live;
///
/// 字幕行的增量事件。
///
/// 必须是**增量**而非全量：1 小时会话约 1200 行，全量每 100ms 序列化一次
/// 等于 2.4MB/s 的无谓开销，足以卡住 UI。
#[derive(Debug, Clone, PartialEq)]
pub enum LineEvent {
    /// 新增一行（此时 `target` 尚无，EN 已可上屏）
    Added {
        seq: u64,
        source: String,
        start_ms: u64,
        end_ms: u64,
        forced_cut: bool,
        /// 说话人标签（多声源时区分「我」与「对方」）
        speaker: Option<String>,
    },
    /// 译文回填
    Filled {
        seq: u64,
        target: String,
        needs_review: bool,
    },
    /// 标记待复核
    Revised { seq: u64 },
}

/// 转写盘写入接口。
///
/// 抽成 trait 是为了让编排层与文件格式解耦：阶段一只接 `NoopSink`
/// 验证链路，阶段二再接真实的 append-only 实现。
///
/// **实现必须是「失败也不致命」的**：写盘失败只降级为仅内存，
/// 绝不能中断转写 —— 用户宁可丢字幕也不能丢掉正在进行的转写会话。
pub trait TranscriptSink: Send {
    /// 一行 EN 就位
    ///
    /// `speaker` 是说话人标签（多声源时区分「我」与「对方」），
    /// 必须落盘 —— 否则从文件回读的历史丢失了对话角色。
    fn append_add(
        &mut self,
        seq: u64,
        source: &str,
        start_ms: u64,
        end_ms: u64,
        forced_cut: bool,
        speaker: &str,
    );

    /// 译文回填（追加 patch 记录，不原地改）
    fn append_fill(&mut self, seq: u64, target: &str, needs_review: bool);

    /// 把缓冲区落盘。转写盘要求「实时」，故不能攒着不写。
    fn flush(&mut self);

    /// 会话结束：最终 flush + 释放句柄
    fn finish(&mut self);

    /// 已成功写入的记录条数（用于状态展示）
    fn written(&self) -> u64 {
        0
    }

    /// 写入失败次数（磁盘满、权限、路径不存在……）
    fn failures(&self) -> u64 {
        0
    }

    /// 最近一次失败原因，供 UI 如实展示
    fn last_error(&self) -> Option<String> {
        None
    }

    /// 本会话的落盘路径。UI 要展示给用户（「转写保存在…」），
    /// 用户还要能自己去取这份文件。
    fn path(&self) -> Option<String> {
        None
    }
}

/// 不落盘。
pub struct NoopSink;

impl TranscriptSink for NoopSink {
    fn append_add(&mut self, _: u64, _: &str, _: u64, _: u64, _: bool, _: &str) {}
    fn append_fill(&mut self, _: u64, _: &str, _: bool) {}
    fn flush(&mut self) {}
    fn finish(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_in_memory_only() {
        // 默认不落盘：调用方不显式给目录就不该在用户桌面乱建文件
        let c = PipelineConfig::default();
        assert!(c.transcript_dir.is_none());
    }

    #[test]
    fn validate_reports_missing_asr_first() {
        let c = PipelineConfig {
            asr_model_dir: PathBuf::from("no-such-asr"),
            ..Default::default()
        };
        let e = c.validate().unwrap_err().to_string();
        assert!(e.contains("ASR"), "{e}");
    }

    #[test]
    fn profile_reaches_asr_and_mt_configs() {
        // 档位此前定义了却无人调用，这里锁定「真的接上了」
        let fast = PipelineConfig {
            profile: PerformanceProfile::Fast,
            mt_accurate: true,
            ..Default::default()
        };
        let stable = PipelineConfig {
            profile: PerformanceProfile::Stable,
            ..Default::default()
        };

        // ⚠️ 已知缺口（阶段二处理）：`AsrConfig::with_profile` 只按
        // `beam > 1` 决定用 greedy 还是 beam search，
        // **beam 数值本身并没有传给引擎**。
        // 所以 Fast(beam 2) 与 Stable(beam 4) 目前都落在
        // modified_beam_search 上，「快速档」并不真的更快。
        // 这里断言的是**当前真实行为**，不是期望行为 —— 改引擎时再改这里。
        assert_eq!(fast.asr_config().decoding_method, "modified_beam_search");
        assert_eq!(stable.asr_config().decoding_method, "modified_beam_search");

        // MT 侧是通的：档位确实影响 tier
        assert_eq!(fast.mt_config().tier, crate::mt::QualityTier::Accurate);
        assert_eq!(stable.mt_config().tier, crate::mt::QualityTier::Fast);
    }

    #[test]
    fn noop_sink_reports_nothing_written() {
        let mut s = NoopSink;
        s.append_add(0, "a", 0, 1, false, "对方");
        s.append_fill(0, "甲", false);
        s.flush();
        s.finish();
        assert_eq!(s.written(), 0);
        assert_eq!(s.failures(), 0);
    }
}