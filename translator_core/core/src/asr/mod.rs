//! ASR 后端抽象。
//!
//! 本模块只定义**与引擎无关**的契约。sherpa-onnx 的具体类型被隔离在
//! `backend.rs` 之后 —— 这样即使某个平台的原生库缺失，核心逻辑与测试
//! 仍可编译运行（用 `MockAsr`）。

use crate::audio::PcmChunk;
use std::sync::Arc;

/// sherpa-onnx 后端需 `asr-sherpa`（Windows static）或
/// `asr-sherpa-android`（Android shared）feature（都会链接原生库）。
/// 未启用时核心逻辑与测试仍可编译（用 `MockAsr` 跑通管线）。
#[cfg(any(feature = "asr-sherpa", feature = "asr-sherpa-android"))]
pub mod sherpa;

/// 离线（非流式）ASR —— 两遍解码的第二遍（需 sherpa feature）。
#[cfg(any(feature = "asr-sherpa", feature = "asr-sherpa-android"))]
pub mod offline;

/// 不依赖原生库的参考实现，用于管线联调与测试。
pub mod mock;

/// 一次识别产出
#[derive(Debug, Clone, PartialEq)]
pub enum AsrEvent {
    /// 中间假设（会大幅改写，**只用于预览态**）
    Partial {
        text: String,
        /// 相对段起点的时间戳（秒）
        timestamp_s: f32,
    },
    /// 断句后的确认结果（唯一可进正式字幕的事件）
    Final {
        text: String,
        start_s: f32,
        end_s: f32,
        tokens: Vec<String>,
    },
    /// 引擎内部断句（sherpa endpoint）。此时应 `reset` 并提交 Final。
    SegmentBoundary,
}

/// 识别引擎配置（与具体引擎无关的部分）
#[derive(Debug, Clone)]
pub struct AsrConfig {
    /// 模型目录（含 transducer 三件套 + tokens.txt）
    pub model_dir: std::path::PathBuf,
    /// 推理线程数，来自 `AppConfig::asr_threads`
    pub num_threads: usize,
    /// greedy_search（快，延迟低）| modified_beam_search（准，慢）
    pub decoding_method: String,
    pub provider: String,
    /// ASR 热词（Layer A）
    pub hotwords: Vec<String>,
    /// 热词打分，越大偏置越强
    pub hotwords_score: f32,
    /// 是否启用引擎内部 endpoint。
    ///
    /// ## 2026-10-23 实测结论：保持**关闭**，断句仍由 VAD 独占
    ///
    /// 2026-10-23 用真实 zipformer 模型 + 真实英语语音（OSR 语料）做过对照实验，
    /// 同一素材下各方案的产出文本量（以「一次喂完不切分」为 100% 上界）：
    ///
    /// | 方案 | 达上界 |
    /// |---|---|
    /// | 每段新建会话（旧实现） | 29% |
    /// | 每段新建 + 静音 padding | 58% |
    /// | 复用会话但丢弃段间静音 | 88% |
    /// | **复用会话 + 音频连续（含段间静音）+ VAD 断句** | **100%** |
    /// | 复用会话 + 音频连续 + sherpa endpoint 断句 | 104% |
    ///
    /// 三条结论：
    /// 1. **病根是「每段新建会话」**，不是「两套断句并存」。
    ///    流式 zipformer 依赖跨段上下文，新 session 没有左上下文，段首内容大面积丢失。
    /// 2. **决定性因素是「音频连续」**（100% vs 88%）——
    ///    段间静音是 sherpa 判断词边界的依据，丢掉它词就会粘连或被截断。
    /// 3. endpoint 开关只带来 4% 差异，却要放弃 VAD 的 `start_ms`/`end_ms` 时间轴
    ///    （sherpa 的 `end_s` 是**相对本句**的，需额外换算），得不偿失。
    ///
    /// 故：断句仍由 VAD 独占（保留精确时间轴），endpoint 保持关闭。
    /// 下面的 rule 参数保留，供将来需要时启用。
    pub enable_endpoint: bool,

    /// endpoint 规则 1：静音超过此时长（秒）且**尚未吐出任何字** → 判定句末。
    pub rule1_min_trailing_silence: f32,
    /// endpoint 规则 2：静音超过此时长（秒）且**已吐出字** → 判定句末。
    pub rule2_min_trailing_silence: f32,
    /// endpoint 规则 3：句子超过此时长（秒）→ 强制断句（防长句不切）。
    pub rule3_min_utterance_length: f32,
}

impl AsrConfig {
    pub fn new(model_dir: impl Into<std::path::PathBuf>, num_threads: usize) -> Self {
        Self {
            model_dir: model_dir.into(),
            num_threads,
            decoding_method: "greedy_search".to_string(),
            provider: "cpu".to_string(),
            hotwords: Vec::new(),
            hotwords_score: 1.5,
            // 2026-10-23：开启。流式模型自带句末检测（尾静 2.4s），
            // 与 VAD 协同（先到先断）——断句比单纯能量 VAD 语义化得多。
            enable_endpoint: true,
            // sherpa 官方默认值（见 python-api-examples 的 endpoint 示例）。
            // 当前 endpoint 关闭，这些参数不生效；保留以便将来启用。
            rule1_min_trailing_silence: 2.4,
            rule2_min_trailing_silence: 1.2,
            rule3_min_utterance_length: 300.0,
        }
    }

    /// 性能档位映射到 beam 宽度
    pub fn with_profile(mut self, beam: u32) -> Self {
        if beam <= 1 {
            self.decoding_method = "greedy_search".to_string();
        } else {
            self.decoding_method = "modified_beam_search".to_string();
        }
        self
    }
}

/// 一个活跃的识别会话。
///
/// ## 生命周期：**整个转写会话只有一个实例**（不是每个 VAD 段一个）
///
/// 2026-10-23 实测（真实 zipformer 模型 + 真实英语语音，OSR 语料）发现：
/// 「每段新建会话 → 整段喂入 → `finish()`」只能认出上界 **29%** 的文本，
/// 且句中词被拦腰截断（`MOOD PLA` / `UCK ROUND`）。
/// 根因：流式 zipformer 依赖**跨段上下文累积** —— 新建的 session 没有左上下文，
/// 段首内容大面积丢失。
///
/// 对照实测（同一模型、同一素材）：
///
/// | 方案 | 产出文本 | 达上界 |
/// |---|---|---|
/// | 每段新建会话（❌ 旧） | 283 字符 | 29% |
/// | 每段新建 + 静音 padding | 570 字符 | 58% |
/// | **长生命周期会话（✅ 本 trait）** | **1017 字符** | **104%** |
/// | 一次喂完不切分（上界参考） | 979 字符 | 100% |
///
/// 结论：**必须复用会话**。补静音 padding 不够，必须让模型看到连续音频。
///
/// ## 事件投递约定
///
/// - `accept` 返回 `Partial`（预览）与 `Final`（引擎判定句子结束）
/// - `finalize_segment` 由上层在**自己的**断句点上调用，取出当前句的最终文本
/// - `finish` 是幂等的：重复调用返回空 vec
pub trait AsrSession: Send {
    /// 送入音频（16 kHz mono f32）。返回本轮产生的事件。
    ///
    /// 返回 `Final` 表示引擎自己判定这一句已结束（sherpa endpoint 规则命中）。
    fn accept(&mut self, samples: &[f32]) -> crate::Result<Vec<AsrEvent>>;
    /// 取出当前累积的最终文本（若已成型），并**重置解码状态**准备下一句。
    ///
    /// 与 `finish` 的区别：`finish` 终结整个会话（不可再用），
    /// `finalize_segment` 只是切走一句，**会话继续存活**，
    /// 下一句仍能利用 sherpa 保留的上下文（官方 `Reset()` 不重置特征提取器）。
    fn finalize_segment(&mut self) -> crate::Result<Vec<AsrEvent>>;
    /// 标记输入结束，冲刷尾部上下文并产出最终结果。幂等。
    fn finish(&mut self) -> crate::Result<Vec<AsrEvent>>;
    /// 当前假设文本（用于 UI 预览，无需额外事件）
    fn partial_text(&self) -> String;
}

/// 可共享的识别器。多会话可复用同一 recognizer 以省内存。
pub trait AsrBackend: Send + Sync {
    /// 用新热词集创建会话
    fn create_session(&self, hotwords: Vec<String>) -> crate::Result<Box<dyn AsrSession>>;
    /// 后端名称（UI 展示与日志）
    fn name(&self) -> &'static str;
    /// 引擎是否可用（模型缺失等）
    fn is_available(&self) -> bool;
    /// 不可用时的原因
    fn unavailable_reason(&self) -> Option<String> {
        None
    }
}

/// 把 PcmChunk 拆成引擎友好的块长。
///
/// Zipformer 256ms 模型以 3200 样本为理想块长（16k×0.2s）。
/// 这里做线性拆分，尾部留待下次累积。
#[derive(Debug)]
pub struct ChunkSplitter {
    target: usize,
    buffer: Vec<f32>,
}

impl ChunkSplitter {
    pub fn new(target: usize) -> Self {
        Self {
            target,
            buffer: Vec::with_capacity(target * 2),
        }
    }

    /// 追加音频，返回可送引擎的块
    pub fn push(&mut self, chunk: &PcmChunk) -> Vec<Vec<f32>> {
        self.buffer.extend_from_slice(&chunk.samples);
        let mut out = Vec::new();
        while self.buffer.len() >= self.target {
            let rest = self.buffer.split_off(self.target);
            out.push(std::mem::replace(&mut self.buffer, rest));
        }
        out
    }

    /// 会话结束时冲刷残留
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }
}

/// 共享热词快照。词库热更新时原子替换，会话创建时读取。
pub type HotwordSet = Arc<Vec<String>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_maps_beam_to_decoding_method() {
        let c = AsrConfig::new("m", 2).with_profile(1);
        assert_eq!(c.decoding_method, "greedy_search");
        let c = AsrConfig::new("m", 2).with_profile(4);
        assert_eq!(c.decoding_method, "modified_beam_search");
    }

    #[test]
    fn endpoint_enabled_by_default() {
        // 2026-10-23 起默认开启：sherpa 的句末检测（尾静 2.4s）比纯能量
        // VAD 语义化，实测文本产出达上界 104%（关闭仅 29%）。
        // 它只在「该说的都说了」时触发，不会把词拦腰截断。
        assert!(AsrConfig::new("m", 2).enable_endpoint);
    }

    #[test]
    fn splitter_emits_target_sized_chunks() {
        let mut s = ChunkSplitter::new(3200);
        let chunk = PcmChunk::new(vec![0.1; 8000], 0);
        let out = s.push(&chunk);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), 3200);
        assert_eq!(s.buffer.len(), 8000 - 6400);
    }

    #[test]
    fn splitter_accumulates_across_chunks() {
        let mut s = ChunkSplitter::new(3200);
        // 第一次 2000 样本，不足一块
        assert!(s.push(&PcmChunk::new(vec![0.1; 2000], 0)).is_empty());
        // 第二次再 2000 → 累计 4000，恰好吐出一块 3200，剩 800
        let out = s.push(&PcmChunk::new(vec![0.1; 2000], 125));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 3200);
        assert_eq!(s.buffer.len(), 800);
    }

    #[test]
    fn splitter_flush_returns_remainder() {
        let mut s = ChunkSplitter::new(3200);
        s.push(&PcmChunk::new(vec![0.1; 1000], 0));
        let f = s.flush().expect("残留应可冲刷");
        assert_eq!(f.len(), 1000);
        assert!(s.flush().is_none(), "冲刷后应为空");
    }

    #[test]
    fn splitter_preserves_no_samples() {
        // 反复拆分不得丢样本
        let total = 10_000;
        let mut s = ChunkSplitter::new(3200);
        let out = s.push(&PcmChunk::new(vec![0.5; total], 0));
        let emitted: usize = out.iter().map(|c| c.len()).sum();
        assert_eq!(emitted + s.buffer.len(), total);
    }
}
