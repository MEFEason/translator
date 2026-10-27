//! WAV 写出（诊断用）。
//!
//! 只支持 16-bit PCM 单声道 —— 诊断导出够用，且不引第三方依赖。
//! 字节序：RIFF/WAVE 规范，小端。

use std::io::Write;

/// 把 f32 样本写成 16-bit PCM 单声道 WAV。
///
/// `rate` 是采样率（Hz）。样本按 [-1, 1] 归一化后裁剪。
pub fn write_wav_16k_mono(
    path: &str,
    rate: u32,
    samples: &[f32],
) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let data_bytes = samples.len() * 2;

    // RIFF 头
    f.write_all(b"RIFF")?;
    f.write_all(&((36 + data_bytes) as u32).to_le_bytes())?;
    f.write_all(b"WAVE")?;

    // fmt 子块（PCM，单声道，16 位）
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // 子块大小
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // 单声道
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 2).to_le_bytes())?; // 字节率 = rate * channels * bits/8
    f.write_all(&2u16.to_le_bytes())?; // 块对齐
    f.write_all(&16u16.to_le_bytes())?; // 位深

    // data 子块
    f.write_all(b"data")?;
    f.write_all(&(data_bytes as u32).to_le_bytes())?;
    for s in samples {
        let v = s.clamp(-1.0, 1.0);
        let i = (v * 32767.0) as i16;
        f.write_all(&i.to_le_bytes())?;
    }
    f.flush()
}
