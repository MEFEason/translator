//! 重采样与格式归一化。
//!
//! 统一目标：16 kHz / 单声道 / f32。
//!
//! 关键设计（DESIGN.md §2.1）：Windows 侧优先让系统用
//! `AUTOCONVERT_SAMPLERATE` 做一次重采样，本模块只做必要的
//! **最后一跳**归一化，避免同一份音频被重采样两次导致音质劣化。

/// 立体声 → 单声道（左右平均）
pub fn stereo_to_mono(interleaved: &[f32]) -> Vec<f32> {
    interleaved.chunks_exact(2).map(|c| (c[0] + c[1]) * 0.5).collect()
}

/// i16 → f32，并归一化到 [-1.0, 1.0]
pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// f32 → i16，饱和裁剪（写 WAV 用）
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

/// 线性插值重采样。
///
/// 刻意不用简单抽取（decimation）：48 kHz → 16 kHz 恰好 3:1 时抽取
/// 会在高频产生混叠，而线性插值在语音频段（<8 kHz）听感上足够，
/// 且实现零依赖、跨平台行为一致 —— 这点对 §7.2 的三端一致性测试很重要。
pub fn resample_linear(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        if idx + 1 < input.len() {
            out.push(input[idx] * (1.0 - frac) + input[idx + 1] * frac);
        } else {
            out.push(input[idx]);
        }
    }
    out
}

/// 峰值归一化到 -1 dBFS，避免削顶失真。
///
/// 只在峰值超过阈值时提升音量，不做压缩，避免动态范围被改变。
pub fn normalize_peak(samples: &mut [f32], target_dbfs: f32) {
    if samples.is_empty() {
        return;
    }
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    if peak <= 1e-6 {
        return; // 静音，不处理
    }
    let target = 10f32.powf(target_dbfs / 20.0);
    // 不降低音量（peak <= target 时不动），只防止削顶
    if peak < target {
        return;
    }
    let gain = target / peak;
    for s in samples.iter_mut() {
        *s *= gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_downmix_averages_channels() {
        let stereo = vec![1.0, 0.0, -1.0, 1.0, 0.5, 0.5];
        let mono = stereo_to_mono(&stereo);
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.5).abs() < 1e-6);
        assert!((mono[1] - 0.0).abs() < 1e-6);
        assert!((mono[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn i16_roundtrip_preserves_signal() {
        let original: Vec<i16> = vec![0, 16384, -16384, 32767, -32768];
        let f = i16_to_f32(&original);
        let back = f32_to_i16(&f);
        // 32767 → 0.99997 → 32766, 容差 1
        for (a, b) in original.iter().zip(back.iter()) {
            assert!((a - b).abs() <= 1, "mismatch {a} vs {b}");
        }
    }

    #[test]
    fn resample_48k_to_16k_gives_third_length() {
        // 1 秒 48 kHz 正弦 → 应得到约 16000 样本
        let n = 48_000;
        let input: Vec<f32> = (0..n)
            .map(|i| (i as f32 * 0.01).sin())
            .collect();
        let out = resample_linear(&input, 48_000, 16_000);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn resample_identity_when_same_rate() {
        let input = vec![0.1, 0.2, 0.3];
        let out = resample_linear(&input, 16_000, 16_000);
        assert_eq!(input, out);
    }

    #[test]
    fn normalize_prevents_clipping() {
        let mut s = vec![2.0, -3.0, 1.0];
        normalize_peak(&mut s, -1.0);
        let peak = s.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        // 应被限到 -1 dBFS ≈ 0.891
        assert!(peak <= 0.892, "peak = {peak}");
        assert!(peak > 0.88);
    }

    #[test]
    fn normalize_leaves_quiet_audio_untouched() {
        let mut s = vec![0.1, -0.2];
        let before = s.clone();
        normalize_peak(&mut s, -1.0);
        assert_eq!(before, s);
    }

    #[test]
    fn normalize_handles_silence_without_dividing_by_zero() {
        let mut s = vec![0.0; 100];
        normalize_peak(&mut s, -1.0);
        assert!(s.iter().all(|&v| v == 0.0));
    }
}
