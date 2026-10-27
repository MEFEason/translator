#![cfg(feature = "asr-sherpa")]
//! 实证测试：真实模型 + 真实语音，决定 ASR 会话架构。
//!
//! ## 背景
//!
//! 2026-10-23 用户反馈「一直在说 Can you read this，他只有这些零星的」。
//! 诊断：VAD 切出 10 段，其中 **8 段识别为空**。
//!
//! 2026-10-23 用真实 zipformer 模型 + 真实英语语音（OSR 语料）实测，
//! 确认根因是 `handle_segment()` **每段新建 ASR 会话**：
//! 新 session 没有左上下文，段首内容大面积丢失
//! （实测：现状方案只认出上界 29% 的文本，且句中词被拦腰截断）。
//!
//! ## 本测试对比的方案
//!
//! | 方案 | 做法 |
//! |------|------|
//! | A | 现状：每 VAD 段新建 session → `finish()` |
//! | B | 上界参考：全部音频一次喂完，不切分 |
//! | C1 | 长生命周期 session + 连续喂 + RMS 静音点提交 + reset |
//! | C3 | 官方 endpoint 模式（rule1=2.4 / rule2=1.2 / rule3=300）|
//! | C5 | 官方 endpoint 模式（rule1=0.8 / rule2=0.6，更灵敏）|
//! | F | 每段新建 session，但段前后补静音 padding（最小改动方案）|
//!
//! 判定标准：**产出文本量**（相对上界 B 的占比）与**句子完整性**。
//!
//! ## 运行
//!
//! ```text
//! cargo test --features asr-sherpa --test asr_context -- --nocapture
//! ```
//!
//! 需要 `ASR_MODEL_DIR` 指向模型目录、`SPEECH_DIR` 指向含 wav 的目录。
//! 没设置时**跳过**（不失败）—— CI 上没有模型是常态。

use std::path::PathBuf;

use translator_core::asr::{AsrBackend, AsrConfig, AsrEvent};
use translator_core::audio::resample::resample_linear;

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
    resample_linear(samples, src_rate, 16_000)
}

/// 按能量粗切段（模拟 VAD：静音间隔处断开）
fn split_by_silence(samples: &[f32], thresh: f32, min_len: usize) -> Vec<Vec<f32>> {
    let frame = 512usize;
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut cur: Vec<f32> = Vec::new();
    let mut quiet_run = 0usize;
    for f in samples.chunks(frame) {
        let rms = (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt();
        if rms < thresh {
            quiet_run += 1;
            if quiet_run >= 12 && cur.len() >= min_len {
                out.push(std::mem::take(&mut cur));
                quiet_run = 0;
            }
        } else {
            quiet_run = 0;
            cur.extend_from_slice(f);
        }
    }
    if cur.len() >= min_len {
        out.push(cur);
    }
    out
}

/// 方案 A：每段新建 session，整段喂入后 finish()（**生产现状**）
fn scheme_a(backend: &dyn AsrBackend, segs: &[Vec<f32>]) -> Vec<String> {
    let mut finals = Vec::new();
    for seg in segs {
        let mut s = backend.create_session(vec![]).expect("create_session");
        let _ = s.accept(seg);
        for ev in s.finish().expect("finish") {
            if let AsrEvent::Final { text, .. } = ev {
                finals.push(text);
            }
        }
    }
    finals
}

/// 方案 F：每段新建 session，但段前后补静音 padding
///
/// 假设：词在段边界被切开，是因为 zipformer 看不到静音边界。
/// 补 padding 后模型能识别出「词结束」，但仍缺左上下文。
fn scheme_f(backend: &dyn AsrBackend, segs: &[Vec<f32>], pad_ms: usize) -> Vec<String> {
    let pad = vec![0.0f32; 16_000 * pad_ms / 1000];
    let mut finals = Vec::new();
    for seg in segs {
        let mut padded = pad.clone();
        padded.extend_from_slice(seg);
        padded.extend_from_slice(&pad);
        let mut s = backend.create_session(vec![]).expect("create_session");
        let _ = s.accept(&padded);
        for ev in s.finish().expect("finish") {
            if let AsrEvent::Final { text, .. } = ev {
                finals.push(text);
            }
        }
    }
    finals
}

/// 方案 B：全部音频一次喂完（**上界参考**）
fn scheme_b(backend: &dyn AsrBackend, segs: &[Vec<f32>]) -> Vec<String> {
    let all: Vec<f32> = segs.iter().flat_map(|s| s.iter().copied()).collect();
    let mut s = backend.create_session(vec![]).expect("create_session");
    let _ = s.accept(&all);
    let mut finals = Vec::new();
    for ev in s.finish().expect("finish") {
        if let AsrEvent::Final { text, .. } = ev {
            finals.push(text);
        }
    }
    finals
}

/// 建一个 sherpa recognizer
fn make_rec(model_dir: &PathBuf, enable_endpoint: bool) -> sherpa_onnx::OnlineRecognizer {
    use sherpa_onnx::OnlineRecognizerConfig;
    let mut c = OnlineRecognizerConfig::default();
    c.model_config.transducer.encoder = Some(
        model_dir.join("encoder-epoch-99-avg-1.int8.onnx").display().to_string(),
    );
    c.model_config.transducer.decoder =
        Some(model_dir.join("decoder-epoch-99-avg-1.onnx").display().to_string());
    c.model_config.transducer.joiner =
        Some(model_dir.join("joiner-epoch-99-avg-1.onnx").display().to_string());
    c.model_config.tokens = Some(model_dir.join("tokens.txt").display().to_string());
    c.model_config.num_threads = 2;
    c.feat_config.sample_rate = 16_000;
    c.feat_config.feature_dim = 80;
    c.decoding_method = Some("modified_beam_search".into());
    c.enable_endpoint = enable_endpoint;
    sherpa_onnx::OnlineRecognizer::create(&c).expect("创建 recognizer")
}

/// 方案 C1：长生命周期 session + 连续喂 + RMS 静音点提交 + reset
fn scheme_c1(model_dir: &PathBuf, pcm: &[f32], quiet_chunks: usize, thresh: f32) -> Vec<String> {
    let recognizer = make_rec(model_dir, false);
    let mut stream = recognizer.create_stream();
    let frame = 3200usize;
    let mut finals: Vec<String> = Vec::new();
    let mut quiet_run = 0usize;

    for chunk in pcm.chunks(frame) {
        stream.accept_waveform(16_000, chunk);
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
        let text = recognizer
            .get_result(&stream)
            .map(|r| r.text)
            .unwrap_or_default();
        let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
        if rms < thresh {
            quiet_run += 1;
        } else {
            quiet_run = 0;
        }
        if quiet_run >= quiet_chunks {
            let t = text.trim().to_string();
            if !t.is_empty() {
                finals.push(t);
            }
            recognizer.reset(&mut stream);
            quiet_run = 0;
        }
    }
    let t = recognizer
        .get_result(&stream)
        .map(|r| r.text)
        .unwrap_or_default();
    let t = t.trim().to_string();
    if !t.is_empty() {
        finals.push(t);
    }
    finals
}

/// 方案 C3 / C5：官方 endpoint 模式
///
/// 严格照抄 sherpa 官方 `speech-recognition-from-microphone-with-endpoint-detection.py`：
/// 连续喂 → `is_endpoint` → 提交 → `reset`。
fn scheme_endpoint(
    model_dir: &PathBuf,
    pcm: &[f32],
    rule1: f32,
    rule2: f32,
    rule3: f32,
) -> Vec<String> {
    use sherpa_onnx::OnlineRecognizerConfig;
    let mut c = OnlineRecognizerConfig::default();
    c.model_config.transducer.encoder = Some(
        model_dir.join("encoder-epoch-99-avg-1.int8.onnx").display().to_string(),
    );
    c.model_config.transducer.decoder =
        Some(model_dir.join("decoder-epoch-99-avg-1.onnx").display().to_string());
    c.model_config.transducer.joiner =
        Some(model_dir.join("joiner-epoch-99-avg-1.onnx").display().to_string());
    c.model_config.tokens = Some(model_dir.join("tokens.txt").display().to_string());
    c.model_config.num_threads = 2;
    c.feat_config.sample_rate = 16_000;
    c.feat_config.feature_dim = 80;
    c.decoding_method = Some("modified_beam_search".into());
    c.enable_endpoint = true;
    c.rule1_min_trailing_silence = rule1;
    c.rule2_min_trailing_silence = rule2;
    c.rule3_min_utterance_length = rule3;

    let recognizer = sherpa_onnx::OnlineRecognizer::create(&c).expect("创建 recognizer");
    let mut stream = recognizer.create_stream();
    let frame = 3200usize;
    let mut finals: Vec<String> = Vec::new();

    for chunk in pcm.chunks(frame) {
        stream.accept_waveform(16_000, chunk);
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
        if recognizer.is_endpoint(&stream) {
            let text = recognizer
                .get_result(&stream)
                .map(|r| r.text)
                .unwrap_or_default();
            let t = text.trim().to_string();
            if !t.is_empty() {
                finals.push(t);
            }
            recognizer.reset(&mut stream);
        }
    }
    // 收尾
    let t = recognizer
        .get_result(&stream)
        .map(|r| r.text)
        .unwrap_or_default();
    let t = t.trim().to_string();
    if !t.is_empty() {
        finals.push(t);
    }
    finals
}


/// 方案 G：**生产改造的确切形状** —— 复用会话 + 按 VAD 段喂 + 段末 finalize
///
/// 与 C3 的区别：
/// - C3：连续喂**全部**音频（含静音），由 sherpa endpoint 决定句末
/// - G：只喂 VAD 切出的**语音段**（丢弃段间静音），
///   由上层 VAD 的断句点调用 `finalize_segment()` 交出一句
///
/// G 的风险：丢掉段间静音 → sherpa 看不到静音边界 → 词可能粘连；
/// 但 G 保留了 VAD 的时间戳（start_ms/end_ms），字幕时轴更准。
fn scheme_g(model_dir: &PathBuf, segs: &[Vec<f32>]) -> Vec<String> {
    let recognizer = make_rec(model_dir, false);
    let mut stream = recognizer.create_stream();
    let mut finals: Vec<String> = Vec::new();

    for seg in segs {
        for chunk in seg.chunks(3200) {
            stream.accept_waveform(16_000, chunk);
            while recognizer.is_ready(&stream) {
                recognizer.decode(&stream);
            }
        }
        // finalize_segment：补尾部静音逼出最后的字 → 取结果 → reset
        let pad = vec![0.0f32; 16_000 * 300 / 1000];
        stream.accept_waveform(16_000, &pad);
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
        if let Some(r) = recognizer.get_result(&stream) {
            let t = r.text.trim().to_string();
            if !t.is_empty() {
                finals.push(t);
            }
        }
        recognizer.reset(&mut stream);
    }
    finals
}


/// 方案 H：**连续喂全部音频**（含段间静音）+ 由上层 VAD 断句点提交
///
/// 与 G 的唯一区别：音频**连续**（不丢段间静音）→ sherpa 能看到词边界。
/// 与 C3 的区别：断句由我们的 VAD 决定，而非 sherpa endpoint。
///
/// 若 H ≈ C3，说明「音频连续」是关键，「谁决定断句」不重要 ——
/// 那就可以保留现有 VAD 时间轴，改动面最小。
fn scheme_h(
    model_dir: &PathBuf,
    pcm: &[f32],
    seg_end_offsets: &[usize],
) -> Vec<String> {
    let recognizer = make_rec(model_dir, false);
    let mut stream = recognizer.create_stream();
    let mut finals: Vec<String> = Vec::new();
    let mut fed = 0usize;

    for &end in seg_end_offsets {
        // 连续喂到该段结束（含段间静音）
        while fed < end {
            let hi = (fed + 3200).min(end);
            stream.accept_waveform(16_000, &pcm[fed..hi]);
            fed = hi;
            while recognizer.is_ready(&stream) {
                recognizer.decode(&stream);
            }
        }
        // VAD 断句点 → 提交并 reset
        let pad = vec![0.0f32; 16_000 * 300 / 1000];
        stream.accept_waveform(16_000, &pad);
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
        if let Some(r) = recognizer.get_result(&stream) {
            let t = r.text.trim().to_string();
            if !t.is_empty() {
                finals.push(t);
            }
        }
        recognizer.reset(&mut stream);
    }
    finals
}

/// 计算每个 VAD 段在**完整音频**中的结束偏移（含段间静音）
fn seg_end_offsets(pcm: &[f32], thresh: f32, min_len: usize) -> Vec<usize> {
    let frame = 512usize;
    let mut ends = Vec::new();
    let mut speech = 0usize;
    let mut quiet = 0usize;
    let mut i = 0usize;
    while i < pcm.len() {
        let f = &pcm[i..(i + frame).min(pcm.len())];
        let rms = (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt();
        if rms < thresh {
            quiet += 1;
            if quiet >= 12 && speech >= min_len {
                ends.push(i + frame);
                speech = 0;
                quiet = 0;
            }
        } else {
            quiet = 0;
            speech += frame;
        }
        i += frame;
    }
    if speech >= min_len {
        ends.push(pcm.len());
    }
    ends
}

/// 方案 C6：官方规则但 rule3 改 20s（长句兜底，避免连续说话 300 秒不出行）
fn scheme_c6(model_dir: &PathBuf, pcm: &[f32]) -> Vec<String> {
    scheme_endpoint(model_dir, pcm, 2.4, 1.2, 20.0)
}

fn model_dir() -> Option<PathBuf> {
    std::env::var("ASR_MODEL_DIR").ok().map(PathBuf::from)
}

#[test]
fn compare_asr_session_architectures() {
    let model_dir = match model_dir() {
        Some(d) if d.is_dir() => d,
        _ => {
            eprintln!("[SKIP] 未设置 ASR_MODEL_DIR，跳过真实模型实证测试");
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
    let backend = translator_core::pipeline::build_asr_backend(
        &cfg,
        translator_core::pipeline::BackendChoice::Auto,
    )
    .expect("加载真实 ASR 模型失败");

    let chars = |v: &Vec<String>| v.iter().map(|s| s.trim().len()).sum::<usize>();
    let mut totals: Vec<(&str, usize)> = vec![
        ("A 现状(每段新建)", 0),
        ("B 上界(一次喂完)", 0),
        ("C1 连续喂+RMS提交", 0),
        ("C3 endpoint官方默认", 0),
        ("C5 endpoint灵敏", 0),
        ("F 每段+静音padding", 0),
        ("G 复用会话+VAD段末", 0),
        ("H 连续喂+VAD断句", 0),
        ("C6 endpoint+rule3=20", 0),
    ];

    for wav in wavs.iter().take(3) {
        let (raw, rate) = read_wav(wav);
        let pcm = to_16k(&raw, rate);
        let segs = split_by_silence(&pcm, 0.01, 8000);

        eprintln!("\n=========== {} ===========", wav.file_name().unwrap().to_string_lossy());
        eprintln!(
            "总长 {:.1}s，VAD 切出 {} 段",
            pcm.len() as f32 / 16000.0,
            segs.len()
        );

        let a = scheme_a(&*backend, &segs);
        let b = scheme_b(&*backend, &segs);
        let c1 = scheme_c1(&model_dir, &pcm, 5, 0.01);
        let c3 = scheme_endpoint(&model_dir, &pcm, 2.4, 1.2, 300.0);
        let c5 = scheme_endpoint(&model_dir, &pcm, 0.8, 0.6, 300.0);
        let f = scheme_f(&*backend, &segs, 300);
        let g = scheme_g(&model_dir, &segs);
        let h = scheme_h(&model_dir, &pcm, &seg_end_offsets(&pcm, 0.01, 8000));
        let c6 = scheme_c6(&model_dir, &pcm);

        for (name, v) in [
            ("A", &a),
            ("B", &b),
            ("C1", &c1),
            ("C3", &c3),
            ("C5", &c5),
            ("F", &f),
            ("G", &g),
            ("H", &h),
            ("C6", &c6),
        ] {
            eprintln!("[{name}] {} 条 / {} 字符", v.len(), chars(v));
            for t in v.iter().take(4) {
                eprintln!("   {name}→ {t:?}");
            }
        }

        totals[0].1 += chars(&a);
        totals[1].1 += chars(&b);
        totals[2].1 += chars(&c1);
        totals[3].1 += chars(&c3);
        totals[4].1 += chars(&c5);
        totals[5].1 += chars(&f);
        totals[6].1 += chars(&g);
        totals[7].1 += chars(&h);
        totals[8].1 += chars(&c6);
    }

    let upper = totals[1].1.max(1);
    eprintln!("\n=========== 汇总（3 个真实语音文件）============");
    for (name, total) in &totals {
        eprintln!(
            "{:24} {:5} 字符   达上界 {:3.0}%",
            name,
            total,
            *total as f64 / upper as f64 * 100.0
        );
    }

    // 断言：长生命周期方案必须显著优于现状
    let a = totals[0].1;
    let best = totals
        .iter()
        .filter(|(n, _)| n.starts_with('C') || n.starts_with('F'))
        .map(|(_, t)| *t)
        .max()
        .unwrap_or(0);
    assert!(
        best > a * 3 / 2,
        "候选方案必须比现状显著更好（A={a}, best={best}）"
    );
}

/// 探针：sherpa 的 start_s/end_s 在 reset 之后是否**全局连续**？
///
/// 这决定字幕时间戳怎么算：
/// - 若全局连续 → 直接用 sherpa 的时间戳
/// - 若每句从 0 重来 → 必须由引擎自己维护时间基准
#[test]
fn probe_timestamp_continuity_across_reset() {
    let model_dir = match model_dir() {
        Some(d) if d.is_dir() => d,
        _ => {
            eprintln!("[SKIP] 未设置 ASR_MODEL_DIR");
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
        eprintln!("[SKIP] 没有 wav");
        return;
    }

    use sherpa_onnx::OnlineRecognizerConfig;
    let mut c = OnlineRecognizerConfig::default();
    c.model_config.transducer.encoder = Some(
        model_dir.join("encoder-epoch-99-avg-1.int8.onnx").display().to_string(),
    );
    c.model_config.transducer.decoder =
        Some(model_dir.join("decoder-epoch-99-avg-1.onnx").display().to_string());
    c.model_config.transducer.joiner =
        Some(model_dir.join("joiner-epoch-99-avg-1.onnx").display().to_string());
    c.model_config.tokens = Some(model_dir.join("tokens.txt").display().to_string());
    c.model_config.num_threads = 2;
    c.feat_config.sample_rate = 16_000;
    c.feat_config.feature_dim = 80;
    c.decoding_method = Some("modified_beam_search".into());
    c.enable_endpoint = true;
    c.rule1_min_trailing_silence = 2.4;
    c.rule2_min_trailing_silence = 1.2;
    c.rule3_min_utterance_length = 300.0;

    let recognizer = sherpa_onnx::OnlineRecognizer::create(&c).expect("创建 recognizer");
    let mut stream = recognizer.create_stream();
    let (raw, rate) = read_wav(&wavs[0]);
    let pcm = to_16k(&raw, rate);

    let mut fed_sec = 0.0f32;
    for chunk in pcm.chunks(3200) {
        stream.accept_waveform(16_000, chunk);
        fed_sec += chunk.len() as f32 / 16_000.0;
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
        if recognizer.is_endpoint(&stream) {
            if let Some(r) = recognizer.get_result(&stream) {
                eprintln!(
                    "句末 @ 已喂 {:.1}s → start_s={:?} end_s={:?} is_final={} text={:?}",
                    fed_sec, r.start_time, r.timestamps.as_ref().and_then(|t| t.last()), r.is_final, r.text
                );
            }
            recognizer.reset(&mut stream);
        }
    }
}
