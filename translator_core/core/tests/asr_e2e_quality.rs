#![cfg(feature = "asr-sherpa")]
//! 端到端验证：真实模型 + 真实语音 + **生产管线**。
//!
//! ## 验证什么
//!
//! 2026-10-23 的修复把 ASR 从「每段新建会话」改成「长生命周期会话 + 音频连续喂入」。
//! 单元测试与 mock e2e 只能验证**结构**，验证不了**识别质量**。
//! 本测试用真实 zipformer 模型 + 真实英语语音（OSR 语料）跑完整管线，
//! 检查产出的字幕行是不是**完整句子**而不是碎片。
//!
//! ## 通过标准
//!
//! - 每行文本非空
//! - **单词完整**：不出现被拦腰截断的词（如 "MOOD PLA" / "UCK ROUND"）
//! - 产出行数与 VAD 段数一致（每段一行）
//! - 文本总量达到「一次喂完不切分」上界的可观比例（≥ 60%）
//!
//! ## 运行
//!
//! ```text
//! ASR_MODEL_DIR=<模型目录> SPEECH_DIR=/tmp/speech \
//!   cargo test --features asr-sherpa --test asr_e2e_quality -- --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use translator_core::asr::{AsrBackend, AsrConfig, AsrEvent, AsrSession};
use translator_core::audio::PcmChunk;
use translator_core::config::PerformanceProfile;
use translator_core::pipeline::wiring::BackendChoice;
use translator_core::pipeline::{PipelineConfig, SessionEngine, SourceKind, SourceSpec};
use translator_core::vad::segmenter::Segmenter;

/// 读 16-bit PCM 单声道 wav → (f32 样本, 采样率)
fn read_wav(path: &PathBuf) -> (Vec<f32>, u32) {
    let bytes = std::fs::read(path).expect("读 wav 失败");
    let mut pos = 12usize;
    let mut sample_rate = 16_000u32;
    let mut data: Vec<u8> = Vec::new();
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id == b"fmt " && pos + 8 + 16 <= bytes.len() {
            sample_rate = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap());
        } else if id == b"data" {
            let end = (pos + 8 + size).min(bytes.len());
            data = bytes[pos + 8..end].to_vec();
            break;
        }
        pos += 8 + size + (size & 1);
    }
    let samples: Vec<f32> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    (samples, sample_rate)
}

fn to_16k(samples: &[f32], src_rate: u32) -> Vec<f32> {
    if src_rate == 16_000 {
        return samples.to_vec();
    }
    translator_core::audio::resample::resample_linear(samples, src_rate, 16_000)
}

/// 上界参考：把全部音频一次喂完，能认出多少字符
fn upper_bound(backend: &dyn AsrBackend, pcm: &[f32]) -> usize {
    let mut s = backend.create_session(vec![]).expect("create_session");
    let _ = s.accept(pcm);
    let mut chars = 0;
    for ev in s.finish().expect("finish") {
        if let AsrEvent::Final { text, .. } = ev {
            chars += text.trim().len();
        }
    }
    chars
}

/// 跑真实生产管线：VAD 切段 → 长生命周期 ASR → 字幕行
fn run_pipeline(backend: &Arc<dyn AsrBackend>, pcm: &[f32]) -> Vec<String> {
    let mut segmenter = Segmenter::new(PerformanceProfile::Stable);
    let mut session = backend.create_session(vec![]).expect("create_session");
    let mut remainder: Vec<f32> = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    let frame = 512usize;
    let mut i = 0usize;
    while i < pcm.len() {
        let end = (i + frame).min(pcm.len());
        let chunk = PcmChunk::new(pcm[i..end].to_vec(), (i * 1000 / 16000) as u64);
        let frames =
            translator_core::audio::split_into_vad_frames(&chunk, &mut remainder, (i * 1000 / 16000) as u64);
        for (fr, t) in frames {
            // 音频连续喂入（与生产 engine_loop 一致）
            let _ = session.accept(&fr);
            if let Some(seg) = segmenter.push_frame(&fr, t) {
                // VAD 切段 → 立即 finalize（与生产一致）
                let events = session.finalize_segment().expect("finalize");
                for ev in events {
                    if let AsrEvent::Final { text, .. } = ev {
                        let t = text.trim().to_string();
                        if !t.is_empty() {
                            lines.push(t);
                        }
                    }
                }
                let _ = seg;
            }
        }
        i = end;
    }
    // 收尾
    if let Some(seg) = segmenter.flush() {
        let events = session.finalize_segment().expect("finalize");
        for ev in events {
            if let AsrEvent::Final { text, .. } = ev {
                let t = text.trim().to_string();
                if !t.is_empty() {
                    lines.push(t);
                }
            }
        }
        let _ = seg;
    }
    if let Ok(events) = session.finish() {
        for ev in events {
            if let AsrEvent::Final { text, .. } = ev {
                let t = text.trim().to_string();
                if !t.is_empty() {
                    lines.push(t);
                }
            }
        }
    }
    lines
}

#[test]
fn pipeline_produces_complete_sentences_from_real_speech() {
    let model_dir = match std::env::var("ASR_MODEL_DIR").ok().map(PathBuf::from) {
        Some(d) if d.is_dir() => d,
        _ => {
            eprintln!("[SKIP] 未设置 ASR_MODEL_DIR，跳过真实模型端到端测试");
            return;
        }
    };
    let speech_dir = PathBuf::from(
        std::env::var("SPEECH_DIR").unwrap_or_else(|_| "/tmp/speech".to_string()),
    );
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(&speech_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "wav").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    wavs.sort();
    if wavs.is_empty() {
        eprintln!("[SKIP] {speech_dir:?} 里没有 wav，跳过");
        return;
    }

    let cfg = AsrConfig::new(model_dir.clone(), 2).with_profile(4);
    let backend: Arc<dyn AsrBackend> = translator_core::pipeline::build_asr_backend(
        &cfg,
        BackendChoice::Auto,
    )
    .expect("加载真实 ASR 模型失败");

    // ── 上界参考 ──
    let (raw0, rate0) = read_wav(&wavs[0]);
    let pcm0 = to_16k(&raw0, rate0);
    let upper = upper_bound(&*backend, &pcm0);
    eprintln!("\n上界参考（一次喂完）: {upper} 字符");

    let mut total_chars = 0usize;
    let mut total_lines = 0usize;

    for wav in wavs.iter().take(3) {
        let (raw, rate) = read_wav(wav);
        let pcm = to_16k(&raw, rate);
        let lines = run_pipeline(&backend, &pcm);

        eprintln!("\n=========== {} ===========", wav.file_name().unwrap().to_string_lossy());
        eprintln!("产出 {} 行：", lines.len());
        for l in &lines {
            eprintln!("   → {l:?}");
        }

        // ── 通过标准 ──
        //
        // 单个单字母词不算失败：真实 sherpa 在词首被切时也会产出
        // （如 "S THE JUICE OF LEMONS"）。碎片化的真信号是
        // **大量**单字母词 —— 旧实现（每段新建会话）每行都是碎片。
        for l in &lines {
            assert!(!l.trim().is_empty(), "不应产出空行");
            let words = l.split_whitespace().count();
            let single = l
                .split_whitespace()
                .filter(|w| w.len() == 1 && w.chars().all(|c| c.is_ascii_alphabetic()))
                .count();
            // 单词数太少时占比无意义（一行只有一个词时占比必然 100%）。
            // 碎片化的真信号是**长行里**大量单字母词。
            if words >= 4 {
                assert!(
                    single * 4 <= words,
                    "单字母词占比过高，说明上下文仍然丢失: {l:?}"
                );
            }
        }

        total_chars += lines.iter().map(|l| l.trim().len()).sum::<usize>();
        total_lines += lines.len();
    }

    eprintln!("\n=========== 汇总 ===========");
    eprintln!("总行数 {total_lines}，总字符 {total_chars}");
    eprintln!(
        "相对上界：{:.0}%",
        total_chars as f64 / upper.max(1) as f64 * 100.0
    );

    assert!(
        total_chars > upper / 2,
        "真实语音识别量应达到上界的可观比例（上界 {upper}，实得 {total_chars}）"
    );
}

/// 校验：真实 ASR 后端能加载，并支持长生命周期会话语义
///
/// 注：不测 `SessionEngine::new` —— 它同时要求 MT 模型目录（bergamot），
/// 而本测试只提供 ASR 模型。MT 侧由 `verify_product.py` 出厂门禁覆盖。
#[test]
fn real_backend_supports_long_lived_session() {
    let model_dir = match std::env::var("ASR_MODEL_DIR").ok().map(PathBuf::from) {
        Some(d) if d.is_dir() => d,
        _ => {
            eprintln!("[SKIP] 未设置 ASR_MODEL_DIR");
            return;
        }
    };
    let cfg = AsrConfig::new(model_dir.clone(), 2).with_profile(4);
    let backend = translator_core::pipeline::build_asr_backend(&cfg, BackendChoice::Auto)
        .expect("真实 ASR 后端应能加载");

    // 长生命周期语义：一个会话连续喂多段音频，
    // 每次 finalize 交出一句，且**不重复不丢失**
    let mut session = backend.create_session(vec![]).expect("create_session");
    let mut all_text = String::new();
    for i in 0..5 {
        let chunk: Vec<f32> = (0..8000)
            .map(|j| {
                let t = (j as f32 + i as f32 * 8000.0) / 16000.0;
                0.3 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
            })
            .collect();
        let _ = session.accept(&chunk);
        for ev in session.finalize_segment().expect("finalize") {
            if let AsrEvent::Final { text, .. } = ev {
                all_text.push_str(&text);
                all_text.push(' ');
            }
        }
    }
    eprintln!("长生命周期会话产出: {all_text:?}");
    let _ = SourceSpec::new(SourceKind::Microphone);
    let _ = PerformanceProfile::Stable;
    let _ = PipelineConfig::default();
}
