//! 音频层：统一 PCM 契约与无锁传输。
//!
//! 三端捕获层（WASAPI / ScreenCaptureKit / AudioPlaybackCapture）
//! 必须全部收敛到本模块定义的 `PcmChunk` 出口。

pub mod agc;
pub mod wav;
pub mod resample;
pub mod ring;

use crate::config::AppConfig;

/// 统一音频块：16 kHz / 单声道 / f32 ∈ [-1.0, 1.0]
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub samples: Vec<f32>,
    /// 单调时钟毫秒，用于对齐字幕时间戳（DESIGN.md §7.2 第 4 条）
    pub capture_time_ms: u64,
    /// 环形缓冲溢出时丢弃的帧数，用于健康度上报
    pub overrun_frames: u32,
}

impl PcmChunk {
    pub fn new(samples: Vec<f32>, capture_time_ms: u64) -> Self {
        Self {
            samples,
            capture_time_ms,
            overrun_frames: 0,
        }
    }

    pub fn duration_ms(&self) -> u64 {
        (self.samples.len() as u64 * 1000) / AppConfig::SAMPLE_RATE as u64
    }

    /// RMS 能量，用于采集自检（判断是否为静音）
    pub fn rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.samples.iter().map(|s| s * s).sum();
        (sum / self.samples.len() as f32).sqrt()
    }

    /// 是否为静音（RMS 低于阈值）
    pub fn is_silent(&self, threshold: f32) -> bool {
        self.rms() < threshold
    }
}

/// 按 VAD 帧长切分。尾部不足一帧的部分保留在 `remainder` 中，
/// 与下一块拼接，避免边界处丢样本。
pub fn split_into_vad_frames(
    chunk: &PcmChunk,
    remainder: &mut Vec<f32>,
    base_time_ms: u64,
) -> Vec<(Vec<f32>, u64)> {
    const FRAME: usize = AppConfig::VAD_FRAME_SAMPLES;

    let mut buf = std::mem::take(remainder);
    buf.extend_from_slice(&chunk.samples);

    let mut frames = Vec::with_capacity(buf.len() / FRAME);
    let mut offset = 0usize;
    while buf.len() - offset >= FRAME {
        let end = offset + FRAME;
        // 每帧 32 ms，用于累加帧起始时间戳
        let frame_time = base_time_ms + (offset as u64 * 1000) / AppConfig::SAMPLE_RATE as u64;
        frames.push((buf[offset..end].to_vec(), frame_time));
        offset = end;
    }
    *remainder = buf[offset..].to_vec();
    frames
}
