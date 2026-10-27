//! VAD 分段器：决定"一句话在哪里结束"。
//!
//! 这是字幕稳定性的核心（DESIGN.md §6.2）。两个设计决定：
//!
//! 1. **hangover 不可省**：语音结束后必须等一段静音才断句。设 0 会导致
//!    VAD 在句中抖动，把一句话切成碎片。
//! 2. **强制切分兜底**：超过 `MAX_SEGMENT_MS` 仍未断句（如背景噪声持续
//!    被判为语音）必须强行切分，否则单段音频无限增长，ASR 延迟与内存同步失控。

use crate::config::{AppConfig, PerformanceProfile};

/// VAD 每帧的判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadDecision {
    /// 检测到语音
    Speech,
    /// 检测到静音
    Silence,
}

/// 一个已完成的语音段
#[derive(Debug, Clone)]
pub struct SpeechSegment {
    /// 16 kHz mono f32
    pub samples: Vec<f32>,
    /// 段起始时间戳（单调时钟 ms）
    pub start_ms: u64,
    /// 段结束时间戳（单调时钟 ms）
    pub end_ms: u64,
    /// 因超过 MAX_SEGMENT_MS 被强制切分
    pub forced: bool,
}

impl SpeechSegment {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// 绝对静音下限（RMS）。
///
/// -66 dBFS。低于此值一定是数字静音（没接设备 / 静音 / 权限问题）。
const ABS_SILENCE_RMS: f32 = 0.0002;

/// 噪声基线估计窗口（帧数）。
///
/// 200 帧 × 32 ms ≈ 6.4 秒。取窗口内 RMS 的**低分位数**当噪声底 ——
/// 标准做法（min-statistics / percentile-based noise floor）。
const NOISE_WINDOW: usize = 200;

/// 取窗口内第几分位作为噪声基线。
///
/// 取 10%：只要窗口里 10% 的帧是「非语音」（呼吸、停顿、底噪），
/// 这个分位数就仍然代表噪声底，而不会被持续语音拉高。
const NOISE_PERCENTILE: usize = 10;

/// 每多少帧重算一次分位数（分位数要排序，10% 频率足够）。
const NOISE_RECALC_EVERY: u32 = 20;

/// 噪声基线的初始值。
///
/// -60 dBFS。开机时还不知道环境噪声，用一个典型值起步，
/// 之后由 [`Segmenter::update_noise_floor`] 快速收敛到实际水平。
const NOISE_FLOOR_INIT: f32 = 0.001;

/// 噪声基线的下限（防止被极安静环境拉到 0，导致任何杂音都算语音）。
const NOISE_FLOOR_MIN: f32 = 0.0002;

/// 送进 ASR 的**最小语音段长度**（ms）。
///
/// ## 为什么必须有这个下限（2026-10-23 定位到的根因之二）
///
/// `handle_segment` 对每个 VAD 段**新建一个 ASR 会话**，整段喂入后
/// 立即 `finish()`（`AsrSession::finish()` 后 stream 不可复用）。
/// 而流式 zipformer 需要**上下文累积**才能解出完整词 ——
/// 喂 0.3 秒的碎片独立解码，模型没有足够左右文，只能吐出
/// 空结果或 "U" / "UL" / "ULAR" 这类碎片。
///
/// 实测诊断：VAD 切出 10 段，其中 **8 段识别为空**。
///
/// 所以段不能太碎：不足 [`MIN_SEGMENT_MS`] 的语音段**不切**，
/// 继续累积到够长（或等到会话结束由 `flush` 冲刷）。
/// 送进 ASR 的最小段长（ms）。不足则不切，继续累积。
/// 2026-10-23：300→1000。300ms 的段 = 一两个词一行，字幕碎到没法读；
/// 1 秒保证每行至少是一个可读的意群。
/// 2026-10-24：1000→600。配合 hangover 降到 800ms，成行更快；600ms 仍
/// 低于一个正常意群的最短时长，不会碎到单词成行。
const MIN_SEGMENT_MS: u64 = 600;

/// 长停顿阈值（ms）：静音超过这么久就**强制切出**当前段。
///
/// 兜底：若语音一直断断续续、总时长不足 [`MIN_SEGMENT_MS`]，
/// 段会一直攒着不切 → 用户什么都看不到。
/// 长停顿意味着「说完了」，此时哪怕段短也要切（宁可碎片也别丢）。
const LONG_PAUSE_MS: u64 = 1500;

/// 语音判定阈值相对噪声基线的倍数。
///
/// 语音能量应显著高于背景噪声 —— 3 倍（≈ +9.5 dB）是个稳妥的起点。
const SPEECH_OVER_NOISE_RATIO: f32 = 3.0;

/// 分段状态机。
///
/// 当前使用**能量 + 过零率**的启发式 VAD。接入 Silero 时只需替换
/// `classify_frame`，其余状态机逻辑（hangover / 强制切分）完全复用 ——
/// 这是把 VAD 抽象成单点的原因。
pub struct Segmenter {
    hangover_ms: u64,
    max_segment_ms: u64,
    /// 送进 ASR 的最小段长（ms）。不足则不切，继续累积。
    min_segment_ms: u64,

    /// 已累积的当前段音频
    buffer: Vec<f32>,
    /// 当前段起始时间戳（尚未开始时为 None）
    seg_start_ms: Option<u64>,
    /// 语音段已持续时长（含当前帧）
    speech_duration_ms: u64,
    /// 连续静音已持续时长
    silence_duration_ms: u64,
    /// 最近一帧时间戳
    last_frame_ms: u64,
    /// **自适应噪声基线**（RMS）。
    ///
    /// 为什么不能只用固定阈值（2026-10-23 定位到的根因）：
    /// 实测笔记本麦克风远场说话，语音帧 RMS 仅约 0.002~0.005，
    /// 而固定阈值 0.008（-42 dBFS）会把**整句话**判成静音
    /// → VAD 一段都不出 → 一个字幕都没有。
    /// 降到 0.002 仍不稳（说话电平因人、因距离而异）。
    ///
    /// 解法：跟随环境噪声自适应。
    /// - 安静房间：基线 ~0.0005 → 阈值 ~0.0015，轻声也能检出
    /// - 嘈杂环境：基线 ~0.01 → 阈值 ~0.03，只有明显高于噪声才算语音
    noise_floor: f32,
    /// 近期帧 RMS 的环形缓冲（用于分位数估计噪声底）。
    noise_history: Vec<f32>,
    /// 环形写入位置
    noise_pos: usize,
    /// 距上次重算分位数过了多少帧
    noise_since_calc: u32,
    /// 累计帧数，供测试与统计
    frames_seen: u64,
}

impl Segmenter {
    pub fn new(profile: PerformanceProfile) -> Self {
        Self::with_hangover(profile.hangover_ms())
    }

    pub fn with_hangover(hangover_ms: u64) -> Self {        Self {
            hangover_ms,
            max_segment_ms: AppConfig::MAX_SEGMENT_MS,
            min_segment_ms: MIN_SEGMENT_MS,
            buffer: Vec::with_capacity(AppConfig::SAMPLE_RATE as usize * 20),
            seg_start_ms: None,
            speech_duration_ms: 0,
            silence_duration_ms: 0,
            last_frame_ms: 0,
            frames_seen: 0,
            noise_floor: NOISE_FLOOR_INIT,
            noise_history: Vec::with_capacity(NOISE_WINDOW),
            noise_pos: 0,
            noise_since_calc: 0,
        }
    }

    /// 帧长对应的时长（ms）
    fn frame_ms() -> u64 {
        (AppConfig::VAD_FRAME_SAMPLES as u64 * 1000) / AppConfig::SAMPLE_RATE as u64
    }

    /// 当前 hangover（毫秒）—— 句尾静音多久后断句。
    pub fn hangover_ms(&self) -> u64 {
        self.hangover_ms
    }

    /// 运行时调整 hangover（2026-10-25：UI 拖动「断句灵敏度」Slider 即时生效）。
    ///
    /// 即时生效、无需重建：`push_frame` 每帧读这个字段，
    /// 所以下一帧起就按新值判断。段进行中改也不会破坏状态机
    /// （只影响「还要等多久静音才切」这一个判断）。
    pub fn set_hangover_ms(&mut self, ms: u64) {
        self.hangover_ms = ms;
    }

    /// 当前的语音判定阈值（RMS）。
    ///
    /// = `max(绝对下限, 噪声基线 × 3)`
    ///
    /// 自适应而非固定 —— 理由见 [`Segmenter::noise_floor`]。
    pub fn speech_threshold(&self) -> f32 {
        ABS_SILENCE_RMS.max(self.noise_floor * SPEECH_OVER_NOISE_RATIO)
    }

    /// 更新噪声基线（**分位数估计**）。
    ///
    /// ## ⚠️ 只允许在**静音帧**上调用（2026-10-23 根因修复）
    ///
    /// 曾经在 push_frame 里对所有帧调用本函数。连续播放音乐/语音时，
    /// 噪声历史被语音帧占满，第 10 分位数仍是音乐电平 →
    /// 阈值被抬到音乐电平之上 → **自掩蔽，vad_segments=0**
    /// （实测：QQ音乐播放中 capture_rms=0.07 但一段都切不出）。
    ///
    /// ## 为什么不用「均值跟踪」（踩过的坑）
    ///
    /// 第一版用「上升慢、下降快」的指数跟踪（上升 0.005/帧）。
    /// 50 帧/秒 → 每秒基线涨 25% 差值 —— 连续说话 2~3 秒，
    /// 基线就被抬到语音电平之上，阈值反过来超过语音电平，
    /// 语音被判静音 → **自掩蔽**。
    /// 实测内放播放 9 秒连续中文语音时 `vad_segments=0`（回归）。
    ///
    /// ## 分位数估计（标准做法，min-statistics）
    ///
    /// 维护最近 [`NOISE_WINDOW`] 帧（≈6.4 秒）的 RMS，
    /// 取第 [`NOISE_PERCENTILE`] 分位数当噪声底。
    /// 只要窗口里有 10% 的帧是非语音，这个分位数就仍是真实噪声底，
    /// **不会被持续语音抬高** —— 连续说话时窗口里始终混着
    /// 词间停顿与呼吸。
    fn update_noise_floor(&mut self, rms: f32) {
        // 环形写入
        if self.noise_history.len() < NOISE_WINDOW {
            self.noise_history.push(rms);
        } else {
            self.noise_history[self.noise_pos] = rms;
            self.noise_pos = (self.noise_pos + 1) % NOISE_WINDOW;
        }
        self.noise_since_calc += 1;
        if self.noise_since_calc < NOISE_RECALC_EVERY {
            return;
        }
        self.noise_since_calc = 0;

        // 窗口还没攒够 → 用已有样本算（样本少时分位数天然偏噪，可接受）
        if self.noise_history.is_empty() {
            return;
        }
        let mut sorted = self.noise_history.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (sorted.len() * NOISE_PERCENTILE / 100).min(sorted.len() - 1);
        let floor = sorted[idx];
        // 单向跟随：只允许基线下降或缓慢上升，避免跳变
        if floor < self.noise_floor {
            self.noise_floor = floor;
        } else {
            self.noise_floor += (floor - self.noise_floor) * 0.05;
        }
        if self.noise_floor < NOISE_FLOOR_MIN {
            self.noise_floor = NOISE_FLOOR_MIN;
        }
    }

    /// 能量 + 过零率启发式 VAD。
    ///
    /// ## 阈值取向：宁可多切，不可漏（用户 2026-10-22 要求）
    ///
    /// 早期用固定阈值 0.008，本意是「漏检只是一句话被合并，
    /// 误检会让句子碎片化」。但实测：
    /// - 播放歌曲/视频 → 伴奏过零率高，大量内容被判静音漏掉
    /// - 笔记本麦克风远场说话 → 语音帧 RMS 仅 0.002~0.005，
    ///   **整句话被判静音，VAD 一段都不出**（诊断：vad_segments=0
    ///   而 capture_max_rms=0.0645、capture_chunks=599、dropped=0）
    ///
    /// 固定阈值无论怎么调都不稳（因人、因距离、因环境而异）。
    /// 现改为**噪声自适应**：阈值跟随环境噪声基线浮动，
    /// 绝对下限压到 -66 dBFS，保证安静环境下轻声也能检出。
    ///
    /// 误检的代价由 ASR 自身兜底（它对非语音输出空/低置信），
    /// 而漏检是用户直接看不到内容 —— 不可接受。
    pub fn classify_frame(&self, frame: &[f32]) -> VadDecision {
        if frame.is_empty() {
            return VadDecision::Silence;
        }
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        if rms < self.speech_threshold() {
            return VadDecision::Silence;
        }

        // 过零率：仅用于滤掉纯高频嘶声（电流声、哨音），
        // 不再用它滤音乐 —— 音乐的过零率本就高，滤它就等于滤内容。
        let mut crossings = 0usize;
        for w in frame.windows(2) {
            if (w[0] < 0.0) != (w[1] < 0.0) {
                crossings += 1;
            }
        }
        let zcr = crossings as f32 / frame.len() as f32;
        // 0.35 → 0.9：只滤极端嘶声。白噪声 zcr 才接近 1.0。
        if zcr > 0.9 {
            return VadDecision::Silence;
        }
        VadDecision::Speech
    }

    /// 送入一帧（512 样本），若完成一段则返回该段。
    pub fn push_frame(&mut self, frame: &[f32], frame_time_ms: u64) -> Option<SpeechSegment> {
        self.frames_seen += 1;
        self.last_frame_ms = frame_time_ms;
        let frame_rms =
            (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        let decision = self.classify_frame(frame);
        // ⚠️ 只在**静音帧**更新噪声基线（启动建窗期例外）。
        // 语音/音乐帧一旦进历史，分位数就被语音电平占据，
        // 阈值反超语音电平 → 连续音乐/说话时一段都切不出
        // （2026-10-23 实测根因：vad_segments=0 而 capture_rms=0.07）。
        // 启动建窗期（前 NOISE_WINDOW 帧）允许任何帧进入，
        // 否则第一条语音前的静音太少、基线建不起来；
        // NOISE_FLOOR_MIN 兜底防止初期被单帧尖峰抬高。
        //
        // ⚠️ 2026-10-25 音乐场景自掩蔽修复（用户实测：周杰伦歌曲
        // 「中间几乎全丢，只有最后一段进转写」）：
        // 旧条件 `decision == Silence` 里 Silence 是按**自适应阈值**
        // （基线×3）判的 —— 伴奏持续偏响时，低于该阈值的伴奏帧
        // 仍远高于真实噪声底，却照样进基线窗口 → 分位数被伴奏电平
        // 占据 → 阈值继续抬高 → 更多帧"被判静音"进基线 —— **正反馈
        // 自掩蔽**，人声被吃掉；曲终伴奏变弱基线才回落，于是只剩
        // 最后一段能转写。
        // 现在基线只收**绝对低能量**帧（< NOISE_FLOOR_UPDATE_CAP）：
        // 音乐/语音帧永不进窗口，基线锚定真实环境噪声底。
        // 代价：环境噪声突变（空调开关）时基线下降快、上升慢
        // （靠启动建窗期 + 单向跟随系数 0.05 兜底，可接受）。
        const NOISE_FLOOR_UPDATE_CAP: f32 = 0.02; // ≈ -34 dBFS
        if frame_rms < NOISE_FLOOR_UPDATE_CAP
            || self.frames_seen <= NOISE_WINDOW as u64
        {
            self.update_noise_floor(frame_rms);
        }
        let dt = Self::frame_ms();

        match decision {
            VadDecision::Speech => {
                if self.seg_start_ms.is_none() {
                    self.seg_start_ms = Some(frame_time_ms);
                    self.speech_duration_ms = 0;
                }
                self.buffer.extend_from_slice(frame);
                self.speech_duration_ms += dt;
                self.silence_duration_ms = 0;

                // 强制切分兜底：段过长
                if self.speech_duration_ms >= self.max_segment_ms {
                    let start = self.seg_start_ms.take().unwrap_or(frame_time_ms);
                    let samples = std::mem::take(&mut self.buffer);
                    self.speech_duration_ms = 0;
                    self.silence_duration_ms = 0;
                    return Some(SpeechSegment {
                        samples,
                        start_ms: start,
                        end_ms: frame_time_ms + dt,
                        forced: true,
                    });
                }
                None
            }
            VadDecision::Silence => {
                // 无活动段时的静音不进入 buffer
                if self.seg_start_ms.is_none() {
                    return None;
                }
                self.silence_duration_ms += dt;
                self.buffer.extend_from_slice(frame);

                if self.silence_duration_ms >= self.hangover_ms {
                    // ⚠️ 段太短**先不切** —— 见 MIN_SEGMENT_MS 的说明：
                    // 碎片送进 ASR 会解不出词（实测 8/10 段识别为空）。
                    // 继续累积，等语音时长够了再切。
                    //
                    // 但若停顿已经很长（说完了），就强制切 ——
                    // 否则断断续续的说话会一直攒着，字幕永远不出现。
                    if self.speech_duration_ms < self.min_segment_ms
                        && self.silence_duration_ms < LONG_PAUSE_MS
                    {
                        return None;
                    }
                    let start = self.seg_start_ms.take().unwrap();
                    let samples = std::mem::take(&mut self.buffer);
                    let end = frame_time_ms + dt;
                    self.speech_duration_ms = 0;
                    self.silence_duration_ms = 0;
                    Some(SpeechSegment {
                        samples,
                        start_ms: start,
                        end_ms: end,
                        forced: false,
                    })
                } else {
                    None
                }
            }
        }
    }

    /// 会话结束时冲刷残留段（用户点了停止，但最后一句还没断句）
    pub fn flush(&mut self) -> Option<SpeechSegment> {
        let start = self.seg_start_ms.take()?;
        let samples = std::mem::take(&mut self.buffer);
        self.speech_duration_ms = 0;
        self.silence_duration_ms = 0;
        Some(SpeechSegment {
            samples,
            start_ms: start,
            end_ms: self.last_frame_ms + Self::frame_ms(),
            forced: false,
        })
    }

    /// 重置状态（切换采集源时调用）
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.seg_start_ms = None;
        self.speech_duration_ms = 0;
        self.silence_duration_ms = 0;
        self.frames_seen = 0;
    }

    pub fn has_pending_segment(&self) -> bool {
        self.seg_start_ms.is_some()
    }

    pub fn pending_sample_count(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::PcmChunk;

    const FRAME: usize = AppConfig::VAD_FRAME_SAMPLES;

    /// 构造一帧 440 Hz 正弦，音量可控
    fn tone_frame(amp: f32, offset: usize) -> Vec<f32> {
        (0..FRAME)
            .map(|i| {
                let t = (offset + i) as f32 / AppConfig::SAMPLE_RATE as f32;
                amp * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect()
    }

    fn silence_frame() -> Vec<f32> {
        vec![0.0; FRAME]
    }

    #[test]
    fn silence_produces_no_segment() {
        let mut s = Segmenter::with_hangover(300);
        for i in 0..50 {
            assert!(s.push_frame(&silence_frame(), i as u64 * 32).is_none());
        }
        assert!(s.flush().is_none());
    }

    #[test]
    fn tone_then_silence_emits_one_segment() {
        let mut s = Segmenter::with_hangover(300);
        let mut offset = 0;
        // 2 秒语音（每帧 32 ms → 62 帧）
        for i in 0..62 {
            assert!(s.push_frame(&tone_frame(0.3, offset), i as u64 * 32).is_none());
            offset += FRAME;
        }
        // 500 ms 静音（> hangover）
        let mut seg = None;
        for i in 62..78 {
            if let Some(s) = s.push_frame(&silence_frame(), i as u64 * 32) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("静音超时后应产生段");
        assert!(!seg.forced);
        assert!(seg.duration_ms() >= 1_900, "duration={}", seg.duration_ms());
    }

    #[test]
    fn brief_gap_does_not_split_segment() {
        // 关键测试：句中 100 ms 停顿不应把一句话切成两段
        let mut s = Segmenter::with_hangover(300);
        let mut offset = 0;
        let mut segments = Vec::new();

        for i in 0..30 {
            if let Some(seg) = s.push_frame(&tone_frame(0.3, offset), i as u64 * 32) {
                segments.push(seg);
            }
            offset += FRAME;
        }
        // 3 帧静音 = 96 ms < 300 ms hangover
        for i in 30..33 {
            if let Some(seg) = s.push_frame(&silence_frame(), i as u64 * 32) {
                segments.push(seg);
            }
        }
        // 恢复语音
        for i in 33..63 {
            if let Some(seg) = s.push_frame(&tone_frame(0.3, offset), i as u64 * 32) {
                segments.push(seg);
            }
            offset += FRAME;
        }
        assert!(
            segments.is_empty(),
            "96ms 停顿不该断句，却产生了 {} 段",
            segments.len()
        );
    }

    #[test]
    fn max_segment_forces_cut() {
        let mut s = Segmenter::with_hangover(300);
        s.max_segment_ms = 1_000; // 缩短便于测试
        let mut offset = 0;
        let mut forced = 0;
        for i in 0..60 {
            // 持续语音 1.9 秒，超过 1 秒上限
            if let Some(seg) = s.push_frame(&tone_frame(0.3, offset), i as u64 * 32) {
                if seg.forced {
                    forced += 1;
                }
            }
            offset += FRAME;
        }
        assert!(forced >= 1, "超过 max_segment_ms 应触发强制切分");
    }

    #[test]
    fn flush_returns_pending_segment() {
        let mut s = Segmenter::with_hangover(300);
        let mut offset = 0;
        for i in 0..20 {
            s.push_frame(&tone_frame(0.3, offset), i as u64 * 32);
            offset += FRAME;
        }
        assert!(s.has_pending_segment());
        let seg = s.flush().expect("flush 应返回残留段");
        assert!(!seg.samples.is_empty());
        assert!(!s.has_pending_segment());
    }

    #[test]
    fn quiet_audio_treated_as_silence() {
        // 极低电平（接近数字静音）判为静音。
        //
        // 注意：这里测的是**原始未增益**的极弱信号。实际链路里
        // 远场轻声会被上游 AGC 放大到 0.1 级再进 VAD，所以
        // 「远场轻声」不会被这条误判为静音 —— 见
        // `agc_then_vad_cuts_distant_speech` 的端到端验证。
        let mut s = Segmenter::with_hangover(300);
        // 0.0002 = 数字静音级（-74 dBFS），比任何真实人声都低
        let quiet = tone_frame(0.0002, 0);
        assert_eq!(s.classify_frame(&quiet), VadDecision::Silence);
    }

    /// 端到端回归：**真实 AGC → 真实 VAD** 链路。
    ///
    /// 2026-10-23 用户反馈「转写仍然不可用」的根因：
    /// 远场麦克风输入电平偏低（RMS≈0.004），VAD 把整句话判成静音。
    ///
    /// 修复：上游加 AGC（语音门控自动增益），把远场语音拉到 0.1 级。
    ///
    /// 本测试模拟真实场景：远场人声（带自然起伏）+ 真实房间底噪，
    /// 经 AGC 处理后喂给 VAD，断言**能稳定切出语音段**（不再一句不出）。
    ///
    /// 这是 AGC 与 VAD 协同的核心保证 —— 若 AGC 没生效或 VAD 阈值
    /// 配错，本测试会失败（段数为 0）。
    #[test]
    fn agc_then_vad_cuts_distant_speech() {
        use crate::audio::agc::Agc;

        let mut agc = Agc::new();
        let mut seg = Segmenter::with_hangover(300);
        let mut t_ms = 0u64;
        let mut remainder: Vec<f32> = Vec::new();
        let mut segments: Vec<crate::vad::segmenter::SpeechSegment> = Vec::new();

        // 模拟 6 秒真实场景：人声（远场，amp≈0.004）+ 房间底噪（RMS≈0.0005），
        // 每 2 秒说 1.2 秒、停顿 0.8 秒。
        let sr = AppConfig::SAMPLE_RATE as f32;
        const NOISE_AMP: f32 = 0.0007;
        let mut speech_osc = 0.0f32;
        let dt = 1.0 / sr;

        // 用固定种子的伪随机生成底噪（确定性，测试可复现）
        let mut seed: u32 = 0x12345;
        let mut noise = || -> f32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed as f32 / u32::MAX as f32 - 0.5) * 2.0 * NOISE_AMP
        };

        // 构造 6 秒音频，逐块（320 样本=20ms）喂
        let total = (6.0 * sr) as usize;
        let block = 320usize;

        for start in (0..total).step_by(block) {
            let end = (start + block).min(total);
            let mut samples = Vec::with_capacity(end - start);
            for i in start..end {
                // 人声：远场 amp≈0.004，带自然幅度起伏（模拟元音/辅音）
                speech_osc += dt * 220.0 * 2.0 * std::f32::consts::PI;
                // 幅度包络：2 秒周期，1.2 秒有声
                let cycle = ((i as f32 / sr) % 2.0) / 2.0;
                let voice_on = cycle < 0.6; // 前 60%（1.2s）有声
                let voice_amp = if voice_on {
                    // 元音起伏：0.004 * (0.6 + 0.4*sin)
                    0.004 * (0.6 + 0.4 * (i as f32 * 0.01).sin())
                } else {
                    0.0
                };
                let voice = voice_amp * (speech_osc.sin());
                samples.push(voice + noise());
            }

            // AGC 处理（语音门控：有人声时放大，静音时回落）
            agc.process(&mut samples);

            // 切 VAD 帧并喂 —— 用生产同款 split_into_vad_frames（带 remainder
            // 跨块累积）。直接 `samples.len()/FRAME` 会丢样本：
            // block=320 < FRAME=512 时一帧都切不出来。
            let chunk = crate::audio::PcmChunk::new(samples, t_ms);
            for (frame, frame_time) in crate::audio::split_into_vad_frames(
                &chunk,
                &mut remainder,
                t_ms,
            ) {
                if let Some(s) = seg.push_frame(&frame, frame_time) {
                    segments.push(s);
                }
                t_ms += 32;
            }
        }

        // 断言：6 秒里 3 段语音，应至少切出 2 段（句间停顿够触发断句）
        assert!(
            segments.len() >= 2,
            "AGC 后 VAD 应能稳定切出远场语音段，实际只切出 {} 段。",
            segments.len()
        );

        // 每段时长应合理（>300ms MIN_SEGMENT），不是碎片
        for s in &segments {
            assert!(
                s.duration_ms() >= 250,
                "段过短（{}ms），疑似碎片而非完整句",
                s.duration_ms()
            );
        }
    }

    #[test]
    fn loud_tone_treated_as_speech() {
        let s = Segmenter::with_hangover(300);
        assert_eq!(
            s.classify_frame(&tone_frame(0.3, 0)),
            VadDecision::Speech
        );
    }

    #[test]
    fn frame_split_preserves_all_samples() {
        // 验证 split_into_vad_frames 不丢样本
        let total = AppConfig::SAMPLE_RATE as usize; // 1 秒
        let chunk = PcmChunk::new(vec![0.25; total], 0);
        let mut remainder = Vec::new();
        let frames = crate::audio::split_into_vad_frames(&chunk, &mut remainder, 0);

        let consumed: usize = frames.iter().map(|(f, _)| f.len()).sum();
        assert_eq!(consumed + remainder.len(), total);
        // 16000 / 512 = 31.25 → 31 整帧
        assert_eq!(frames.len(), 31);
        assert_eq!(remainder.len(), 16000 - 31 * 512);
    }
}
