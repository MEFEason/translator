#![cfg(feature = "asr-sherpa")]
//! 双语模型（zipformer-bilingual-zh-en）中文识别实证。
//!
//! ## 背景（2026-10-23）
//!
//! 用户反馈「转写和翻译仍然不可用」，最终根因：之前用的是**纯英文**
//! ASR（zipformer-en-20M），而用户场景是中文音频（歌曲/会议）——
//! 英文模型对中文一个字都认不出。
//!
//! 本测试用**生产代码路径**（`build_asr_backend` → `probe_asr_dir`
//! 自动探测 bpe.vocab → 设置 cjkchar+bpe）加载双语模型，
//! 喂官方中文测试音频，断言**产出含中文字符**。
//!
//! ## 运行
//!
//! ```text
//! ASR_MODEL_DIR=<双语模型目录> SPEECH_DIR=<含中文 wav 的目录> \
//!   cargo test --features asr-sherpa --test asr_bilingual -- --nocapture
//! ```
//! 环境变量未设置时跳过（不失败），避免拖垮常规 CI。

use std::path::PathBuf;

use translator_core::asr::{AsrBackend, AsrConfig, AsrEvent};
use translator_core::pipeline::wiring::BackendChoice;
use translator_core::pipeline::build_asr_backend;

/// 读 16-bit PCM wav → (f32 样本, 采样率)
fn read_wav(path: &PathBuf) -> (Vec<f32>, u32) {
    let data = std::fs::read(path).expect("读 wav 失败");
    // 极简 RIFF 解析：采样率在 24 字节处，数据在 "data" 块
    let rate = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let pos = data
        .windows(4)
        .position(|w| w == *b"data")
        .expect("无 data 块");
    let start = pos + 8;
    let bytes = &data[start..];
    let samples: Vec<f32> = bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    (samples, rate)
}

/// 线性插值重采样到 16k
fn to_16k(samples: &[f32], src_rate: u32) -> Vec<f32> {
    if src_rate == 16000 {
        return samples.to_vec();
    }
    let ratio = src_rate as f64 / 16000.0;
    let out_len = (samples.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 * ratio;
            let i0 = src.floor() as usize;
            let i1 = (i0 + 1).min(samples.len() - 1);
            let frac = (src - i0 as f64) as f32;
            samples[i0] * (1.0 - frac) + samples[i1] * frac
        })
        .collect()
}

fn cjk_count(text: &str) -> usize {
    text.chars()
        .filter(|c| {
            let cp = *c as u32;
            (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
        })
        .count()
}

#[test]
fn bilingual_model_recognizes_chinese() {
    let model_dir = match std::env::var("ASR_MODEL_DIR").ok().map(PathBuf::from) {
        Some(d) if d.is_dir() => d,
        _ => {
            eprintln!("[SKIP] 未设置 ASR_MODEL_DIR");
            return;
        }
    };
    let speech_dir = PathBuf::from(std::env::var("SPEECH_DIR").unwrap_or("/tmp/asr-bi/test_wavs".into()));
    let wavs: Vec<PathBuf> = std::fs::read_dir(&speech_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "wav").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    if wavs.is_empty() {
        eprintln!("[SKIP] SPEECH_DIR 里没有 wav");
        return;
    }

    // 生产路径：build_asr_backend → probe_asr_dir（自动探测 bpe.vocab）
    let cfg = AsrConfig::new(model_dir.clone(), 2).with_profile(4);
    let backend = build_asr_backend(&cfg, BackendChoice::Auto)
        .expect("双语 ASR 后端应能加载（含 bpe 配置）");

    let mut total_cjk = 0usize;
    let mut total_chars = 0usize;
    for wav in wavs.iter().take(4) {
        let (raw, rate) = read_wav(wav);
        let pcm = to_16k(&raw, rate);
        let mut session = backend.create_session(vec![]).expect("create_session");
        let mut text = String::new();
        for chunk in pcm.chunks(3200) {
            let _ = session.accept(chunk).unwrap();
        }
        for ev in session.finish().expect("finish") {
            if let AsrEvent::Final { text: t, .. } = ev {
                text.push_str(&t);
            }
        }
        let cjk = cjk_count(&text);
        total_cjk += cjk;
        total_chars += text.chars().count();
        eprintln!("[{}] 中文{}字 总{}字 → {:?}", wav.file_name().unwrap().to_string_lossy(), cjk, text.chars().count(), text);
    }

    // 断言：4 条中文音频至少认出 10 个汉字（官方 test_wavs 全是中文）
    assert!(
        total_cjk >= 10,
        "双语模型应能识别中文，实际只认出 {total_cjk} 个汉字（总输出 {total_chars} 字）。\
         可能 bpe 配置没生效。"
    );
    eprintln!("✓ 中文识别通过：{total_cjk} 汉字 / {total_chars} 总字符");
}
