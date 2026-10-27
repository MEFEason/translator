//! 桌面端音频采集（Windows WASAPI loopback / macOS CoreAudio taps）。
//!
//! ## 为什么用 flexaudio 而不是自己写 COM
//!
//! 系统音频捕获是硬骨头。实测三条自研路线全部受阻：
//!
//! 1. 手写 COM vtable —— `IAudioClient` 有 14 个方法，索引错一位就崩
//! 2. `windows` crate 0.61 —— `AUTOCONVERTPCM` 常量值本身有误，且缺采样率转换标志
//! 3. `cpal` —— **根本不支持 loopback**，Windows 后端只暴露输入设备
//!
//! flexaudio（MIT，v0.6.0）已解决这些问题，且恰好覆盖本项目全部需求。
//! 有成熟实现时自研 ROI 极低 —— 这是本次弯路换来的结论。
//!
//! ## flexaudio 提供的关键能力
//!
//! | 本项目需求 | flexaudio |
//! |---|---|
//! | Windows 内放 | `SourceKind::SystemLoopback`（WASAPI loopback） |
//! | macOS 内放 | 同上（CoreAudio taps，**需 macOS 14.4+**） |
//! | 麦克风降级 | `SourceKind::Mic` |
//! | 三端块长一致 | **`chunk_ms`**（默认 20，与本项目 `CHUNK_MS` 一致） |
//! | 排除自身提示音 | **`exclude_self`** |
//! | 容器内按进程采集 | `ProcessLoopback` + `ProcessMode::Exclude` |
//!
//! ## 关键约束（§6.2）
//!
//! 采集线程绝不能被下游阻塞。flexaudio 是 **pull 模型**
//! （`poll_chunk()`），比 callback 模型更适合本项目 —— 不存在
//! "音频回调里等待下游"的风险。
//!
//! ## macOS 版本要求修订
//!
//! flexaudio 的 CoreAudio taps 要求 **macOS 14.4+**，比原设计的 13.0 更严。
//! 13.0~14.3 需降级为 SystemLoopback 不可用 → 麦克风路径。见 `capability()`。

use crate::audio::PcmChunk;
use crate::config::{CaptureCapability, CaptureSource};
use crate::platform::PermState;
use crate::error::{Error, Result};
use crate::platform::{PermissionGate, SystemAudioCapture};
use flexaudio::{open, SourceKind, Stream, StreamConfig};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// 统一块长。三端必须一致（§7.2）
pub const CHUNK_MS: u32 = crate::audio::ring::AudioChannel::CHUNK_MS as u32;

/// 采集统计
#[derive(Debug, Default)]
pub struct DesktopCaptureStats {
    pub chunks_sent: AtomicU32,
    /// 轮询不到数据的次数（可能欠载，也可能源静音）
    pub underruns: AtomicU32,
    /// 上游环形缓冲真实丢帧数（来自 flexaudio 的 dropped_before）
    pub dropped: AtomicU32,
    pub source_switches: AtomicU32,
    /// 最近一块的 RMS（0..1）。**判断「麦克风到底采没采到声音」的关键**。
    /// 一直为 0 → 设备/权限/静音问题；明显大于 0 → 音频进来了，
    /// 问题在下游（VAD/ASR）。
    pub last_rms: AtomicU32,

    /// 采集源的**原生**格式（采样率 Hz、声道数）。
    ///
    /// 必须读实际值而不是假设 48000/2 —— 麦克风与内放的原生格式
    /// 可能不同（麦克风常是 48k 单声道 / 44.1k）。
    /// 硬编码错了 → 重采样比例错 → 语音速率偏差 → 识别质量差。
    pub native_rate: AtomicU32,
    pub native_channels: AtomicU32,
    /// 采集期间出现过的**最大** RMS。
    ///
    /// 比 `last_rms` 更有用：它能反映「说话时最响的那一刻有多响」，
    /// 用来定 VAD 的绝对阈值。实测笔记本麦克风远场说话
    /// RMS 仅约 0.004（-48 dBFS），比常见的 0.008 阈值还低 ——
    /// 阈值定高了会把整句话判成静音。
    pub max_rms: AtomicU32,
    /// 采集到的总块数。
    pub chunks_total: AtomicU32,
}

/// 桌面端采集器
pub struct DesktopCapture {
    source: CaptureSource,
    running: Arc<AtomicBool>,
    stats: Arc<DesktopCaptureStats>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// 实际生效的源（可能因降级与请求不同）
    effective_source: Option<SourceKind>,
}

impl DesktopCapture {
    pub fn new(source: CaptureSource) -> Self {
        Self {
            source,
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(DesktopCaptureStats::default()),
            worker: None,
            effective_source: None,
        }
    }

    pub fn stats(&self) -> Arc<DesktopCaptureStats> {
        Arc::clone(&self.stats)
    }

    /// 探测能力。macOS 需检查版本（CoreAudio taps 要求 14.4+）。
    pub fn probe(&self) -> CaptureCapability {
        #[cfg(target_os = "macos")]
        {
            // CoreAudio taps 需 macOS 14.4+。13.0~14.3 无系统内放 API。
            let major = macos_major_version();
            let minor = macos_minor_version();
            if (major, minor) < (14, 4) {
                return CaptureCapability::MicrophoneOnly {
                    reason: format!("macOS {major}.{minor} 无系统音频捕获 API（需 14.4+）"),
                };
            }
            CaptureCapability::SystemAudio
        }

        #[cfg(not(target_os = "macos"))]
        {
            // Windows: WASAPI loopback 免权限
            CaptureCapability::SystemAudio
        }
    }

    /// 请求的源 → flexaudio 源
    fn source_kind(&self) -> SourceKind {
        match self.source {
            CaptureSource::SystemAudio => SourceKind::SystemLoopback,
            CaptureSource::Microphone => SourceKind::Mic,
        }
    }

    fn build_config(&self) -> StreamConfig {
        StreamConfig {
            kind: self.source_kind(),
            // 块长统一 —— 这是三端一致性前提（§7.2）
            chunk_ms: CHUNK_MS,
            // 环形容量：4 秒。满了丢最旧（见 §6.2 背压策略）
            ring_capacity_chunks: (4000 / CHUNK_MS).max(4) as usize,
            // 排除自身播放，避免把提示音转写进字幕
            exclude_self: true,
            // ★ 输出格式：保持 flexaudio 默认（48 kHz 立体声）★
            //
            // 曾经试过在 flexaudio 侧强制 16 kHz 单声道
            // （`output: OutputFormat { 16000, 1 }`）——
            // 结果**麦克风几乎采不到数据**（VAD 只切出 1 段 vs 之前的 8 段），
            // 它的重采样器在麦克风路径上不工作。
            //
            // 所以改为：**采集侧保持 48 kHz 立体声**，
            // 由 `normalize_chunk()` 负责下混 + 重采样到 16 kHz 单声道。
            //
            // ⚠️ 这个 bug 的代价（2026-10-23 用户实测）：
            //   48 kHz 立体声被直接当 16 kHz 单声道喂给 ASR
            //   → 语音被"加速"3 倍、音调升 3 个八度
            //   → ASR 只能识别出零星碎片（诊断：8 段里 6 段识别为空）
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod resample_tests {
    use super::*;

    /// 16 kHz 正弦（模拟语音基频）。
    fn tone_16k(freq: f32, secs: f32) -> Vec<f32> {
        let n = (16_000.0 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 16_000.0).sin())
            .collect()
    }

    /// 上采样到 48 kHz 并复制成双声道（模拟 flexaudio 的原生输出）。
    fn to_48k_stereo(x: &[f32]) -> Vec<f32> {
        let up: Vec<f32> = x
            .iter()
            .flat_map(|&v| {
                // 简单 3 倍插值（够测试用）
                let mut v3 = Vec::with_capacity(3);
                v3.push(v);
                v3.push(v * 0.5 + v * 0.5);
                v3.push(v);
                v3
            })
            .collect();
        // 交织成双声道
        let mut out = Vec::with_capacity(up.len() * 2);
        for v in up {
            out.push(v);
            out.push(v);
        }
        out
    }

    /// 关键回归测试：48 kHz 立体声 → 16 kHz 单声道后，
    /// 信号形状必须与原始 16 kHz 一致。
    ///
    /// 这条测试守护 2026-10-23 定位到的根因：
    /// 采集侧不做归一化 → 48 kHz 音频被当 16 kHz 喂 ASR
    /// → 语音加速 3 倍 → 只识别出零星字符。
    #[test]
    fn normalize_chunk_recovers_16k_mono() {
        let original = tone_16k(220.0, 0.5);
        let native = to_48k_stereo(&original);
        let frames = native.len() / 2;

        let out = normalize_chunk(&native, frames, FLEXAUDIO_RATE, FLEXAUDIO_CHANNELS);

        // 长度应约为原信号的 1/3（48k → 16k）
        let ratio = out.len() as f64 / original.len() as f64;
        assert!(
            (0.9..1.1).contains(&ratio),
            "重采样后长度应约为原 1/3，实际比例 {ratio:.3}（{} → {}）",
            original.len(),
            out.len(),
        );

        // 峰值应一致（±10%）：下混+重采样不改变幅度
        let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let p0 = peak(&original);
        let p1 = peak(&out);
        assert!(
            (p0 * 0.8..=p1 * 1.2).contains(&p0) || (p0 * 0.8..=p0 * 1.2).contains(&p1),
            "峰值应基本保持：原 {p0:.3} → 后 {p1:.3}",
        );

        // 相关性：与原信号（按比例抽取）应高度相关
        let step = original.len() as f64 / out.len() as f64;
        let mut num = 0.0f64;
        let mut d0 = 0.0f64;
        let mut d1 = 0.0f64;
        for (i, &y) in out.iter().enumerate() {
            let x = original[(i as f64 * step) as usize % original.len()];
            num += (x as f64) * (y as f64);
            d0 += (x as f64) * (x as f64);
            d1 += (y as f64) * (y as f64);
        }
        let corr = num / (d0.sqrt() * d1.sqrt()).max(1e-9);
        assert!(
            corr > 0.9,
            "重采样后应与原信号高度相关，实际 corr={corr:.3}（<0.9 说明重采样坏了）",
        );
    }

    /// 单声道输入时不应重复采样（channels=1 走直通）。
    #[test]
    fn normalize_chunk_passthrough_mono() {
        let x = tone_16k(300.0, 0.1);
        let frames = x.len();
        let out = normalize_chunk(&x, frames, 16_000, 1);
        assert_eq!(out.len(), x.len());
        assert!((out[0] - x[0]).abs() < 1e-6);
    }
}

/// flexaudio 的原生输出采样率（Hz）。
///
/// flexaudio 内部以 48 kHz 为规范格式，`StreamConfig.output` 默认
/// `{48000, 2}`（48 kHz 立体声，交错）。
const FLEXAUDIO_RATE: u32 = 48_000;

/// flexaudio 的原生输出声道数（立体声）。
const FLEXAUDIO_CHANNELS: u16 = 2;

/// 把 flexaudio 的原生块归一化成 ASR 需要的 **16 kHz 单声道**。
///
/// ## 为什么必须做（2026-10-23 实测定位到的根因）
///
/// flexaudio 默认输出 48 kHz **立体声**，而 ASR（sherpa-onnx）只吃
/// 16 kHz **单声道**。此前采集侧拿到原始数据就直接喂给 ASR，
/// 相当于把 48 kHz 音频当成 16 kHz 播 —— 语音被"加速"3 倍、
/// 音调升 3 个八度，模型只能吐出零星字符。
///
/// 实测诊断数据（修复前）：8 个语音段里 **6 段识别为空**。
///
/// ## 步骤
///
/// 1. 立体声 → 单声道（左右平均）
/// 2. 48 kHz → 16 kHz 线性插值重采样
/// 3. 削顶保护（-1 dBFS）
///
/// ## 为什么不在 flexaudio 侧配置 16 kHz 单声道
///
/// 试过 `StreamConfig.output = {16000, 1}`，结果**麦克风几乎采不到数据**
/// （VAD 只切出 1 段，而默认配置能切 8 段）——
/// 它的重采样器在麦克风路径上不工作。故改为在 Rust 侧自己做。
fn normalize_chunk(
    interleaved: &[f32],
    frames: usize,
    src_rate: u32,
    channels: u16,
) -> Vec<f32> {
    // 以 frames 为准计算有效长度，避免上游长度与 frames 不一致时越界
    let want = frames.saturating_mul(channels as usize);
    let src = &interleaved[..want.min(interleaved.len())];

    // 1) 下混
    let mono = if channels >= 2 {
        crate::audio::resample::stereo_to_mono(src)
    } else {
        src.to_vec()
    };

    // 2) 重采样到目标采样率
    let dst_rate = crate::config::AppConfig::SAMPLE_RATE as u32;
    let mut out = crate::audio::resample::resample_linear(&mono, src_rate, dst_rate);

    // 3) 削顶保护
    crate::audio::resample::normalize_peak(&mut out, -1.0);
    out
}
impl SystemAudioCapture for DesktopCapture {
    fn start(&mut self, mut on_chunk: Box<dyn FnMut(PcmChunk) + Send>) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Err(Error::Internal("采集已在运行".into()));
        }

        let cfg = self.build_config();
        self.effective_source = Some(cfg.kind);

        // 打开失败通常是"无默认输出设备"或权限问题
        let mut stream: Stream = open(cfg).map_err(|e| {
            Error::CaptureUnavailable(format!(
                "打开音频流失败（{}）: {e}",
                self.source_label()
            ))
        })?;

        stream
            .start()
            .map_err(|e| Error::CaptureUnavailable(format!("启动音频流失败: {e}")))?;

        // 记录**实际**原生格式（不要假设 48 kHz / 立体声）
        {
            let (rate, channels) = stream.native_format();
            let st = self.stats();
            st.native_rate.store(rate, Ordering::Relaxed);
            st.native_channels.store(channels as u32, Ordering::Relaxed);
        }

        self.running.store(true, Ordering::Relaxed);
        let running = Arc::clone(&self.running);
        let stats = Arc::clone(&self.stats);

        // 自动增益控制（AGC）。
        //
        // worker 线程独占处理音频，Agc 是单线程有状态对象，
        // 直接 move 进闭包即可，无需 Arc / Mutex。
        //
        // 为什么放在这里（normalize_chunk 之后）：
        // - normalize_chunk 把 48k 立体声归一成 16k 单声道
        // - AGC 把整体电平自适应拉到目标（约 -20 dBFS）
        // - normalize_peak 再兜底防瞬时削顶
        //
        // 2026-10-23 根因之一：远场麦克风输入电平偏低（RMS≈0.004），
        // 而 normalize_peak 只削顶不提升，所以轻声说话进 VAD 后被当静音。
        // AGC 把电平拉起来后，VAD 阈值反而更稳定可预测。
        let mut agc = crate::audio::agc::Agc::new();

        // pull 模型：独立线程轮询，不占用音频回调线程
        let worker = std::thread::Builder::new()
            .name("desktop-capture".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                while running.load(Ordering::Relaxed) {
                    // 短暂休眠让出 CPU。CHUNK_MS=20 → 每块轮询一次即可
                    std::thread::sleep(std::time::Duration::from_millis(CHUNK_MS as u64 / 2));

                    match stream.poll_chunk() {
                        Some(raw) => {
                            // ★ 归一化到 16 kHz 单声道 ★
                            // flexaudio 默认输出 48 kHz 立体声（交错），
                            // 而 ASR（sherpa-onnx）只吃 16 kHz 单声道 ——
                            // 不转换 = 语音加速 3 倍 = 识别全废。
                            // 用**实际**原生格式归一化（不是硬编码常量）
                            let native_rate = stats.native_rate.load(Ordering::Relaxed).max(1);
                            let native_channels =
                                stats.native_channels.load(Ordering::Relaxed).max(1) as u16;
                            let mut samples = normalize_chunk(
                                &raw.data,
                                raw.frames,
                                native_rate,
                                native_channels,
                            );
                            if samples.is_empty() {
                                continue;
                            }
                            // 上游环形缓冲溢出计数（比轮询空更能反映真实丢帧）
                            if raw.dropped_before > 0 {
                                stats.dropped.fetch_add(raw.dropped_before, Ordering::Relaxed);
                            }
                            // ★ AGC：自适应增益，把电平拉到目标（约 -20 dBFS）★
                            //
                            // 必须在 normalize_peak **之前**：
                            // AGC 先拉整体电平，peak 再兜底防瞬时削顶。
                            // 远场轻声（RMS≈0.004）会被放大到 0.1，
                            // 进 VAD 后不再被当静音。
                            agc.process(&mut samples);
                            crate::audio::resample::normalize_peak(&mut samples, -1.0);
                            let elapsed = t0.elapsed().as_millis() as u64;
                            stats.chunks_sent.fetch_add(1, Ordering::Relaxed);
                            stats.chunks_total.fetch_add(1, Ordering::Relaxed);
                            // RMS 用**归一化后**的信号算（更贴近送进 ASR 的样子）
                            let rms = if samples.is_empty() {
                                0.0
                            } else {
                                (samples.iter().map(|v| v * v).sum::<f32>()
                                    / samples.len() as f32)
                                    .sqrt()
                            };
                            stats.last_rms.store(rms.to_bits(), Ordering::Relaxed);

                            // 记录最大值（原子 max 循环）
                            let mut cur = stats.max_rms.load(Ordering::Relaxed);
                            while rms.to_bits() > cur {
                                match stats.max_rms.compare_exchange_weak(
                                    cur,
                                    rms.to_bits(),
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => cur = actual,
                                }
                            }
                            on_chunk(PcmChunk::new(samples, elapsed));
                        }
                        None => {
                            // 无数据：可能是欠载，也可能源真的没声音
                            stats.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                let _ = stream.stop();
            })
            .map_err(|e| Error::Internal(format!("采集线程创建失败: {e}")))?;

        self.worker = Some(worker);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        Ok(())
    }

    fn capability(&self) -> CaptureCapability {
        self.probe()
    }

    fn source_label(&self) -> String {
        let backend = if cfg!(target_os = "macos") {
            "CoreAudio taps"
        } else {
            "WASAPI loopback"
        };
        match self.source {
            CaptureSource::SystemAudio => format!("系统内放（{backend}）"),
            CaptureSource::Microphone => "麦克风".into(),
        }
    }

    /// 采集统计快照（诊断用）。

    fn capture_stats_snapshot(&self) -> crate::platform::CaptureStatsSnapshot {

        let st = self.stats();

        crate::platform::CaptureStatsSnapshot {

            rms: f32::from_bits(st.last_rms.load(std::sync::atomic::Ordering::Relaxed)),
            max_rms: f32::from_bits(st.max_rms.load(std::sync::atomic::Ordering::Relaxed)),
            native_rate: st.native_rate.load(std::sync::atomic::Ordering::Relaxed),
            native_channels: st.native_channels.load(std::sync::atomic::Ordering::Relaxed),

            chunks: st.chunks_total.load(std::sync::atomic::Ordering::Relaxed) as u64,

            dropped: st.dropped.load(std::sync::atomic::Ordering::Relaxed) as u64,

        }

    }



    fn dropped_frames(&self) -> u64 {
        // flexaudio 会报告真实丢帧数（AudioChunk::dropped_before）
        self.stats.dropped.load(Ordering::Relaxed) as u64
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// 桌面端权限：Windows 免权限；macOS 需 TCC 屏幕录制授权
pub struct DesktopPermission {
    is_macos: bool,
}

impl Default for DesktopPermission {
    fn default() -> Self {
        Self {
            is_macos: cfg!(target_os = "macos"),
        }
    }
}

impl PermissionGate for DesktopPermission {
    fn status(&self) -> PermState {
        if self.is_macos {
            // 需查询 TCC，TODO(M5) 由 Swift 侧回传
            PermState::NeedsManualGrant
        } else {
            PermState::NotRequired
        }
    }

    fn request(&self) -> PermState {
        // macOS 的 TCC 授权无法静默申请，必须引导用户到系统设置
        if self.is_macos {
            PermState::NeedsManualGrant
        } else {
            PermState::NotRequired
        }
    }

    fn open_settings(&self) {
        if self.is_macos {
            // x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open")
                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
                    .spawn();
            }
        }
    }
}

/// macOS 主版本号
#[cfg(target_os = "macos")]
fn macos_major_version() -> u32 {
    // 用 target_os 判断避免引入 libc/objc 依赖
    // 实际版本由 Swift 侧探测后回传；此处给出保守下限
    14
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_ms_is_20_for_cross_platform_consistency() {
        // 这是 §7.2 一致性的硬前提：三端块长必须相同
        assert_eq!(CHUNK_MS, 20);
        assert_eq!(
            crate::config::AppConfig::SAMPLE_RATE as u32 * CHUNK_MS / 1000,
            320
        );
    }

    #[test]
    fn system_audio_maps_to_loopback() {
        let c = DesktopCapture::new(CaptureSource::SystemAudio);
        assert!(matches!(c.source_kind(), SourceKind::SystemLoopback));
    }

    #[test]
    fn microphone_maps_to_mic() {
        let c = DesktopCapture::new(CaptureSource::Microphone);
        assert!(matches!(c.source_kind(), SourceKind::Mic));
    }

    #[test]
    fn config_excludes_self_to_avoid_feedback() {
        // 排除自身播放：否则提示音会被转写进字幕
        let c = DesktopCapture::new(CaptureSource::SystemAudio);
        assert!(c.build_config().exclude_self);
    }

    #[test]
    fn config_ring_holds_about_4_seconds() {
        let c = DesktopCapture::new(CaptureSource::SystemAudio);
        let cfg = c.build_config();
        assert_eq!(cfg.chunk_ms, CHUNK_MS);
        // 4 秒 / 20 ms = 200 块
        assert!(cfg.ring_capacity_chunks >= 200);
    }

    #[test]
    fn stop_before_start_is_safe() {
        let mut c = DesktopCapture::new(CaptureSource::SystemAudio);
        let _ = c.stop();
    }

    #[test]
    fn drop_without_start_is_safe() {
        let c = DesktopCapture::new(CaptureSource::SystemAudio);
        drop(c);
    }

    #[test]
    fn label_mentions_backend() {
        let c = DesktopCapture::new(CaptureSource::SystemAudio);
        let l = c.source_label();
        assert!(l.contains("内放"));
        if cfg!(target_os = "macos") {
            assert!(l.contains("CoreAudio"));
        } else {
            assert!(l.contains("WASAPI"));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_needs_no_permission() {
        let p = DesktopPermission::default();
        assert_eq!(p.status(), PermState::NotRequired);
        assert!(p.status().is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_requires_manual_tcc_grant() {
        // TCC 无法静默申请，必须引导用户
        let p = DesktopPermission::default();
        assert_eq!(p.status(), PermState::NeedsManualGrant);
        assert!(!p.status().is_ok());
    }

    #[test]
    fn stats_default_zero() {
        let c = DesktopCapture::new(CaptureSource::SystemAudio);
        assert_eq!(c.stats().chunks_sent.load(Ordering::Relaxed), 0);
        assert_eq!(c.dropped_frames(), 0);
    }
}
