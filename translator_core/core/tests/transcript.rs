//! 转写盘集成测试。
//!
//! 锁定四条硬判据（对应用户需求 N2/N3/N4）：
//! ① 大量写入后能完整读回，顺序正确
//! ② 译文 patch 正确合并
//! ③ 断电截断只丢最后一条，不崩溃
//! ④ 内存窗口丢弃旧行后，**转写盘仍保留全部内容**（历史可回溯）
//!
//! 另外验证「实时性」：会话进行中文件里就该有内容，而不是等结束才写。

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use translator_core::audio::PcmChunk;
use translator_core::config::{AppConfig, PerformanceProfile};
use translator_core::pipeline::{
    BackendChoice, PipelineConfig, SessionEngine, StartOutcome,
};
use translator_core::pipeline::TranscriptSink; // append_add / append_fill / finish 是 trait 方法
use translator_core::transcript::{list_sessions, query, replay, TranscriptWriter};

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp(tag: &str) -> Tmp {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("translator-tr-e2e-{tag}-{uniq}"));
    std::fs::create_dir_all(&p).unwrap();
    Tmp(p)
}

fn speech_bursts(count: usize) -> Vec<f32> {
    let sr = AppConfig::SAMPLE_RATE as usize;
    let mut out = Vec::new();
    let mut phase = 0.0f32;
    let w = 2.0 * std::f32::consts::PI * 440.0 / sr as f32;
    for _ in 0..count {
        for _ in 0..(sr * 4 / 5) {
            out.push(0.3 * phase.sin());
            phase += w;
        }
        out.extend(std::iter::repeat_n(0.0f32, sr / 2));
    }
    out
}

#[test]
fn write_5000_records_and_read_back_in_order() {
    // 判据 ①
    let t = tmp("5000");
    let mut w = TranscriptWriter::create_in(&t.0).unwrap();
    for i in 0..5000u64 {
        w.append_add(
            i,
            &format!("第 {i} 行 we deploy Kubernetes"),
            i * 100,
            i * 100 + 50,
            i % 7 == 0,
            "对方",
        );
    }
    w.finish();

    let f = list_sessions(&t.0).unwrap().pop().unwrap();
    let r = replay(&f).unwrap();
    assert_eq!(r.lines.len(), 5000, "5000 条必须一条不少");
    assert!(!r.truncated_tail);

    for (i, l) in r.lines.iter().enumerate() {
        assert_eq!(l.seq, i as u64, "seq 必须连续且升序");
        assert_eq!(l.source, format!("第 {i} 行 we deploy Kubernetes"));
        assert_eq!(l.forced_cut, i % 7 == 0);
    }
}

#[test]
fn fill_patches_merge_into_source_lines() {
    // 判据 ②
    let t = tmp("patch");
    let mut w = TranscriptWriter::create_in(&t.0).unwrap();
    for i in 0..100u64 {
        w.append_add(i, &format!("line {i}"), i * 10, i * 10 + 5, false, "对方");
    }
    // 译文乱序到达（模拟 MT 线程并发）
    for i in (0..100u64).rev() {
        w.append_fill(i, &format!("第 {i} 行译文"), i % 11 == 0);
    }
    w.finish();

    let f = list_sessions(&t.0).unwrap().pop().unwrap();
    let r = replay(&f).unwrap();
    assert_eq!(r.lines.len(), 100);
    for (i, l) in r.lines.iter().enumerate() {
        assert_eq!(
            l.target.as_deref(),
            Some(format!("第 {i} 行译文").as_str()),
            "译文必须合并回正确的行"
        );
        assert_eq!(l.needs_review, i % 11 == 0, "待复核标记必须正确");
    }
}

#[test]
fn power_cut_loses_only_the_last_record() {
    // 判据 ③
    let t = tmp("cut");
    let mut w = TranscriptWriter::create_in(&t.0).unwrap();
    for i in 0..50u64 {
        w.append_add(i, &format!("line {i}"), i * 10, i * 10 + 5, false, "对方");
    }
    w.flush(); // 落盘
    // 模拟写到一半断电：直接砍文件尾部
    let f = list_sessions(&t.0).unwrap().pop().unwrap();
    let mut bytes = std::fs::read(&f).unwrap();
    bytes.truncate(bytes.len() - 7);
    std::fs::write(&f, &bytes).unwrap();

    let r = replay(&f).unwrap();
    assert!(r.truncated_tail, "应识别出截断");
    assert_eq!(r.lines.len(), 49, "只应丢最后一条，前 49 条必须完好");
    assert_eq!(r.lines[48].seq, 48);
    assert_eq!(r.lines[48].source, "line 48");
}

#[test]
fn memory_window_drops_but_disk_keeps_everything() {
    // 判据 ④ —— 这是 N3 与 N4 的核心保证
    let t = tmp("window");
    let dir = t.0.join("out");

    let cfg = PipelineConfig {
        backend: BackendChoice::Mock,
        profile: PerformanceProfile::Stable,
        memory_window_lines: 100, // 内存只留 100 行
        ..Default::default()
    };
    let mut engine = SessionEngine::new(&cfg).unwrap();

    let writer = TranscriptWriter::create_in(&dir).unwrap();
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * 20) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(
        engine.start_with_sink(rx, Box::new(writer)),
        StartOutcome::Started
    );

    let mut capture_ms = 0u64;
    // 300 段 → 远超窗口上限
    for block in speech_bursts(300).chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += 20;
    }
    drop(tx);
    std::thread::sleep(Duration::from_millis(600));
    let summary = engine.stop().unwrap();

    // 内存：只有 100 行
    assert!(
        summary.lines.len() <= 100,
        "内存窗口应不超过 100 行，实际 {}",
        summary.lines.len()
    );
    assert!(summary.stats.asr_finals > 100, "但产出的行数应远超窗口");

    // 磁盘：全部都在
    let f = list_sessions(&dir).unwrap().pop().unwrap();
    let r = replay(&f).unwrap();
    assert_eq!(
        r.lines.len() as u64,
        summary.stats.asr_finals,
        "转写盘必须保留全部行 —— 内存丢的不能一起丢"
    );
    assert!(
        r.lines.len() > 100,
        "落盘行数应多于内存窗口，才有回溯价值"
    );
    // 且最老的一行还在（seq 0）
    assert_eq!(r.lines[0].seq, 0, "最早的记录也必须保留");
}

#[test]
fn transcript_is_readable_while_session_is_running() {
    // 实时性（N2）：会话进行中文件里就该有内容
    let t = tmp("live");
    let dir = t.0.join("out");
    let cfg = PipelineConfig {
        backend: BackendChoice::Mock,
        ..Default::default()
    };
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let writer = TranscriptWriter::create_in(&dir).unwrap();
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * 20) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(
        engine.start_with_sink(rx, Box::new(writer)),
        StartOutcome::Started
    );

    let mut capture_ms = 0u64;
    for block in speech_bursts(20).chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += 20;
    }
    std::thread::sleep(Duration::from_millis(400));

    // 还没 stop，文件里就该已经有内容
    let f = list_sessions(&dir).unwrap().pop().unwrap();
    let r = replay(&f).unwrap();
    assert!(
        !r.lines.is_empty(),
        "会话进行中转写盘就该有内容（实时写），实际为空"
    );

    drop(tx);
    let _ = engine.stop().unwrap();
}

#[test]
fn query_pages_history_backwards() {
    // N4：向上滚动分页取回更早的内容
    let t = tmp("page");
    let mut w = TranscriptWriter::create_in(&t.0).unwrap();
    for i in 0..300u64 {
        w.append_add(i, &format!("line {i}"), i * 10, i * 10 + 5, false, "对方");
        w.append_fill(i, &format!("译 {i}"), false);
    }
    w.finish();
    let f = list_sessions(&t.0).unwrap().pop().unwrap();

    // 第一页：最新 50 条（seq 250~299）
    let newest = query(&f, Some(250), 50).unwrap();
    assert_eq!(newest.lines.len(), 50);
    assert_eq!(newest.lines[0].seq, 250);

    // 第二页：再往前 50 条（seq 200~249）
    let older = query(&f, Some(200), 50).unwrap();
    assert_eq!(older.lines.len(), 50);
    assert_eq!(older.lines[0].seq, 200);

    // 两页不重叠
    for l in &older.lines {
        assert!(l.seq < 250, "分页不应重叠");
    }
    // 译文也跟着回来了
    assert_eq!(older.lines[0].target.as_deref(), Some("译 200"));
}

#[test]
fn writer_reports_its_path_for_ui() {
    // UI 要展示「转写保存在…」并让用户能自己取
    let t = tmp("path");
    let w = TranscriptWriter::create_in(&t.0).unwrap();
    let p = w.path().to_path_buf();
    assert!(p.exists());
    assert_eq!(p.extension().unwrap(), "dsht");
    assert!(
        p.file_name().unwrap().to_string_lossy().starts_with("transcript-"),
        "{p:?}"
    );
}

#[test]
fn session_summary_exposes_transcript_path() {
    // 引擎摘要必须带上转写盘路径，FFI 才能告诉 UI
    let t = tmp("summary");
    let dir = t.0.join("out");
    let cfg = PipelineConfig {
        backend: BackendChoice::Mock,
        ..Default::default()
    };
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let writer = TranscriptWriter::create_in(&dir).unwrap();
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * 20) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(
        engine.start_with_sink(rx, Box::new(writer)),
        StartOutcome::Started
    );
    let mut capture_ms = 0u64;
    for block in speech_bursts(4).chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += 20;
    }
    drop(tx);
    std::thread::sleep(Duration::from_millis(250));
    let summary = engine.stop().unwrap();

    let p = summary.transcript_path.expect("摘要应带转写盘路径");
    assert!(Path::new(&p).exists(), "{p} 应存在");
    assert!(summary.transcript_written > 0, "应确实写了内容");
    assert_eq!(summary.transcript_failures, 0);
}

// ══ 导出格式（TXT 对话记录）══════════════════════════════

/// 完整链路：引擎 → 转写盘 → 读回 → 格式化为 TXT
///
/// **不依赖环境音频** —— 用 mock 后端灌合成音频，
/// 断言导出内容含时间戳、说话人、原文、译文。
#[test]
fn export_txt_roundtrip_from_transcript_file() {
    use translator_core::asr::mock::MockBackend;
    use translator_core::audio::PcmChunk;
    use translator_core::config::{AppConfig, PerformanceProfile};
    use translator_core::mt::mock::MockTranslator;
    use translator_core::mt::QualityTier;
    use translator_core::pipeline::{
        BackendChoice, NoopSink, PipelineConfig, SessionEngine, SourceKind, SourceSpec,
        StartOutcome,
    };
    use translator_core::transcript::format_replay;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("translator-export-{uniq}"));
    let _t = Tmp(dir.clone());

    let cfg = PipelineConfig {
        backend: BackendChoice::Mock,
        profile: PerformanceProfile::Stable,
        ..Default::default()
    };
    let mut engine = SessionEngine::with_backends(
        &cfg,
        std::sync::Arc::new(MockBackend::new()),
        std::sync::Arc::new(MockTranslator::new(QualityTier::Fast)) as translator_core::mt::SharedMt,
    )
    .unwrap();

    // 合成 2 段语音
    let sr = AppConfig::SAMPLE_RATE as usize;
    let chunk_len = sr * 20 / 1000;
    let mut audio = Vec::new();
    let mut phase = 0.0f32;
    let w = 2.0 * std::f32::consts::PI * 440.0 / sr as f32;
    for _ in 0..2 {
        for _ in 0..(sr * 4 / 5) {
            audio.push(0.3 * phase.sin());
            phase += w;
        }
        audio.extend(std::iter::repeat_n(0.0f32, sr / 2));
    }

    let (tx, rx) = mpsc::channel::<PcmChunk>();
    let mut capture_ms = 0u64;
    for block in audio.chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += 20;
    }
    drop(tx);

    let writer = TranscriptWriter::create_in(&dir).unwrap();
    assert_eq!(
        engine.start_multi(
            vec![(rx, SourceSpec::new(SourceKind::SystemAudio))],
            Box::new(writer),
        ),
        StartOutcome::Started
    );
    std::thread::sleep(Duration::from_millis(300));
    let _summary = engine.stop().unwrap();

    // 读回 → 格式化
    let f = list_sessions(&dir).unwrap().pop().unwrap();
    let replay = translator_core::transcript::replay(&f).unwrap();
    assert!(!replay.lines.is_empty(), "转写盘应有内容");

    let txt = format_replay(&replay.lines, 2);
    assert!(txt.contains("EN: "), "TXT 应含原文行: {txt}");
    assert!(txt.contains("ZH: "), "TXT 应含译文行: {txt}");
    assert!(txt.contains("对方"), "TXT 应含说话人: {txt}");
    assert!(txt.contains('['), "TXT 应含时间戳: {txt}");

    // SRT / VTT 仍可用
    let srt = format_replay(&replay.lines, 0);
    assert!(srt.contains(" --> "), "{srt}");
    let vtt = format_replay(&replay.lines, 1);
    assert!(vtt.starts_with("WEBVTT"), "{vtt}");
}

#[test]
fn export_txt_marks_speaker_of_each_source() {
    // 说话人必须随行落盘 —— 否则从文件回读的历史分不清谁说的
    use translator_core::transcript::{format_replay, TranscriptLine};
    let lines = vec![
        TranscriptLine {
            seq: 0,
            source: "a".into(),
            speaker: "对方".into(),
            target: Some("甲".into()),
            start_ms: 0,
            end_ms: 1000,
            needs_review: false,
            forced_cut: false,
        },
        TranscriptLine {
            seq: 1,
            source: "b".into(),
            speaker: "我".into(),
            target: Some("乙".into()),
            start_ms: 1000,
            end_ms: 2000,
            needs_review: false,
            forced_cut: false,
        },
    ];
    let txt = format_replay(&lines, 2);
    assert!(txt.contains("对方"), "{txt}");
    assert!(txt.contains("我"), "{txt}");
    // 两个说话人都出现，且各自对应自己的行
    let remote = txt.find("对方").unwrap();
    let local = txt.find("] 我").unwrap();
    assert!(remote < local, "顺序应按时间: {txt}");
}
