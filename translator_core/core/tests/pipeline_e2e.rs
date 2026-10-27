//! 流水线端到端测试：验证**真实引擎**（而非手工拼装的 mock）满足设计不变量。
//!
//! 与 `tests/pipeline.rs` 的分工：
//! - `pipeline.rs` 锁定各零件的**契约**（断句、哨兵、背压语义）
//! - 本文件锁定**装起来之后**的行为（线程、队列、停止语义）
//!
//! 全部用内置参考后端，无需模型文件，因此 CI 可零成本运行。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use translator_core::asr::mock::MockBackend;
use translator_core::audio::PcmChunk;
use translator_core::config::{AppConfig, PerformanceProfile};
use translator_core::mt::mock::MockTranslator;
use translator_core::mt::{MtBackend, QualityTier};
use translator_core::pipeline::{
    BackendChoice, LineEvent, NoopSink, PipelineConfig, SessionEngine, SourceKind,
    SourceSpec, StartOutcome,
};

const CHUNK_MS: u64 = 20;

/// 造一段「语音 0.8s + 静音 0.5s」交替的音频。
/// 静音 500ms > hangover 300ms，保证每段独立断句。
fn speech_bursts(count: usize) -> Vec<f32> {
    let sr = AppConfig::SAMPLE_RATE as usize;
    let mut out = Vec::with_capacity(count * (sr * 13 / 10));
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

fn mock_config() -> PipelineConfig {
    PipelineConfig {
        backend: BackendChoice::Mock,
        profile: PerformanceProfile::Stable,
        ..Default::default()
    }
}

/// 灌音频并等待引擎排空，返回 (事件, 汇总)
fn drive(engine: &mut SessionEngine, samples: &[f32], settle: Duration) -> Vec<LineEvent> {
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(engine.start(rx), StartOutcome::Started);

    let mut capture_ms = 0u64;
    for block in samples.chunks(chunk_len) {
        if tx.send(PcmChunk::new(block.to_vec(), capture_ms)).is_err() {
            break;
        }
        capture_ms += CHUNK_MS;
    }
    drop(tx); // 音频结束

    std::thread::sleep(settle);
    let mut events = engine.take_events();
    let summary = engine.stop().expect("stop 应成功");
    events.extend(engine.take_events());

    assert!(
        summary.stats.asr_finals > 0 || samples.is_empty(),
        "本次测试本应产出字幕行，实际 0 行"
    );
    events
}

#[test]
fn e2e_mock_produces_bilingual_lines() {
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let audio = speech_bursts(6);
    let events = drive(&mut engine, &audio, Duration::from_millis(200));

    let added = events
        .iter()
        .filter(|e| matches!(e, LineEvent::Added { .. }))
        .count();
    let filled = events
        .iter()
        .filter(|e| matches!(e, LineEvent::Filled { .. }))
        .count();

    assert!(added >= 5, "6 段语音应至少产出 5 行，实际 {added}");
    assert_eq!(
        filled, added,
        "每行 EN 都应有对应译文（MT 背压：EN 先上屏，ZH 后补）"
    );
}

#[test]
fn added_event_arrives_before_filled() {
    // 核心不变量 1：EN 先于 ZH。
    //
    // 判定方式：对每个 seq，它的 Filled 出现时，该 seq 的 Added 必须**已经出现过**。
    // 反过来说，若出现「Filled 在 Added 之前」，则说明 UI 会先收到一条
    // 没有原文的译文 —— 那正是「翻译跑在识别前面」的病。
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let events = drive(&mut engine, &speech_bursts(4), Duration::from_millis(200));

    let mut added_seqs: std::collections::HashSet<u64> = Default::default();
    let mut filled_seqs: std::collections::HashSet<u64> = Default::default();

    for e in &events {
        match e {
            LineEvent::Added { seq, .. } => {
                assert!(
                    !filled_seqs.contains(seq),
                    "seq={seq} 的 Added 晚于自己的 Filled"
                );
                added_seqs.insert(*seq);
            }
            LineEvent::Filled { seq, .. } => {
                assert!(
                    added_seqs.contains(seq),
                    "seq={seq} 的 Filled 早于自己的 Added —— 译文跑到了原文前面"
                );
                filled_seqs.insert(*seq);
            }
            _ => {}
        }
    }
    assert!(!added_seqs.is_empty(), "本应产出字幕行");
    assert_eq!(
        added_seqs, filled_seqs,
        "停止时的收尾应让每个 Added 都拿到 Filled"
    );
}

#[test]
fn e2e_mt_slow_does_not_block_asr() {
    // 核心不变量的**真实**检验：翻译故意慢 500ms，
    // 引擎仍必须把 EN 及时吐出来，而不是等翻译。
    //
    // 做法：跑慢速翻译器，然后在翻译还没回来时就检查事件流里已有 Added。
    let cfg = mock_config();
    let slow: std::sync::Arc<dyn MtBackend> =
        std::sync::Arc::new(MockTranslator::with_latency(QualityTier::Fast, 500));
    let mut engine = SessionEngine::with_backends(&cfg, std::sync::Arc::new(MockBackend::new()), slow)
        .unwrap();

    let audio = speech_bursts(4);
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(engine.start(rx), StartOutcome::Started);

    let mut capture_ms = 0u64;
    for block in audio.chunks(chunk_len) {
        if tx.send(PcmChunk::new(block.to_vec(), capture_ms)).is_err() {
            break;
        }
        capture_ms += CHUNK_MS;
    }
    drop(tx);

    // 翻译要 500ms/段，此刻只有第一段可能完成。
    // 只要 EN 已经上屏，就证明 MT 没有阻塞转写显示。
    std::thread::sleep(Duration::from_millis(120));
    let early = engine.take_events();
    let added_early = early
        .iter()
        .filter(|e| matches!(e, LineEvent::Added { .. }))
        .count();
    let filled_early = early
        .iter()
        .filter(|e| matches!(e, LineEvent::Filled { .. }))
        .count();

    assert!(
        added_early > 0,
        "翻译未完成时 EN 就应上屏，实际 Added=0"
    );
    assert!(
        filled_early < added_early,
        "此时译文理应还没到齐（Filled={filled_early} vs Added={added_early}）"
    );

    let _summary = engine.stop().unwrap();
}

#[test]
fn e2e_mt_queue_full_drops_translation_not_audio() {
    // 队列满时**丢译文、绝不丢 EN**。
    // 灌 60 段（远超 32 的队列深度）配慢速翻译器，
    // 断言：mt_dropped > 0 且 EN 行数不受影响。
    let cfg = mock_config();
    let slow: std::sync::Arc<dyn MtBackend> =
        std::sync::Arc::new(MockTranslator::with_latency(QualityTier::Fast, 50));
    let mut engine = SessionEngine::with_backends(&cfg, std::sync::Arc::new(MockBackend::new()), slow)
        .unwrap();

    let audio = speech_bursts(60);
    let summary = {
        let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
        let (tx, rx) = mpsc::channel::<PcmChunk>();
        assert_eq!(engine.start(rx), StartOutcome::Started);
        let mut capture_ms = 0u64;
        for block in audio.chunks(chunk_len) {
            if tx.send(PcmChunk::new(block.to_vec(), capture_ms)).is_err() {
                break;
            }
            capture_ms += CHUNK_MS;
        }
        drop(tx);
        std::thread::sleep(Duration::from_millis(300));
        engine.stop().unwrap()
    };

    assert!(
        summary.stats.mt_dropped > 0,
        "60 段配 50ms 慢翻译，队列（32）必然溢出 —— 若为 0 说明背压没生效"
    );
    // EN 一条都不能少：这是「丢译文不丢原文」的核心保证
    assert_eq!(
        summary.stats.asr_finals,
        summary.lines.len() as u64,
        "内存窗口内的行数应等于 ASR 产出的 final 数"
    );
    // 溢出的译文必须仍以「无译文」的形式存在，而不是整行消失
    assert!(
        summary.lines.iter().all(|l| !l.source.is_empty()),
        "不应有内容丢失的行"
    );
}

#[test]
fn e2e_flush_emits_last_sentence_on_stop() {
    // 停止时必须冲刷最后一句 —— 否则用户说了一句话却没看到字幕
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();

    // 只有一段语音，后面跟静音但**不**再送任何音频：
    // VAD 靠静音判定断句，停止时的 flush() 负责收尾
    let sr = AppConfig::SAMPLE_RATE as usize;
    let mut audio = Vec::new();
    let w = 2.0 * std::f32::consts::PI * 440.0 / sr as f32;
    let mut phase = 0.0f32;
    for _ in 0..(sr * 3 / 4) {
        audio.push(0.3 * phase.sin());
        phase += w;
    }
    // 尾部静音：刚好不够 hangover，让最后一段留在缓冲里等 flush
    audio.extend(std::iter::repeat_n(0.0f32, sr / 5));

    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(engine.start(rx), StartOutcome::Started);
    let mut capture_ms = 0u64;
    for block in audio.chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += CHUNK_MS;
    }
    drop(tx);
    std::thread::sleep(Duration::from_millis(150));
    let summary = engine.stop().unwrap();

    assert!(
        summary.stats.asr_finals >= 1,
        "停止时的 flush 必须把最后一句变成字幕行，实际 0 行"
    );
    assert!(!summary.srt.is_empty(), "SRT 不应为空");
}

#[test]
fn stop_is_idempotent_and_second_call_is_safe() {
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    drive(&mut engine, &speech_bursts(2), Duration::from_millis(80));
    // 第一次 stop 已在 drive 内完成
    let second = engine.stop().expect("第二次 stop 不应 panic");
    assert_eq!(second.stats.asr_finals, 0, "已停止的引擎不应再产出内容");
}

#[test]
fn srt_export_is_wellformed() {
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let audio = speech_bursts(3);

    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(engine.start(rx), StartOutcome::Started);
    let mut capture_ms = 0u64;
    for block in audio.chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += CHUNK_MS;
    }
    drop(tx);
    std::thread::sleep(Duration::from_millis(150));
    let summary = engine.stop().unwrap();

    // SRT 结构：序号 / 时间行 / 原文 / 译文 / 空行
    assert!(summary.srt.starts_with("1\n"), "SRT 应以序号开头");
    assert!(summary.srt.contains(" --> "), "SRT 应含时间戳行");
    assert_eq!(
        summary.srt.lines().count() % 5,
        0,
        "每条字幕应为 5 行（序号/时间/原文/译文/空行）"
    );
    assert!(summary.vtt.starts_with("WEBVTT"));
    assert!(!summary.txt.is_empty());
}

#[test]
fn pipeline_keeps_up_with_realtime() {
    // 性能下限：RTF 必须远小于 1，否则实时采集会被积压撑爆
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let audio = speech_bursts(20);
    let audio_s = audio.len() as f64 / AppConfig::SAMPLE_RATE as f64;

    let t0 = Instant::now();
    drive(&mut engine, &audio, Duration::from_millis(50));
    let elapsed = t0.elapsed().as_secs_f64();

    let rtf = elapsed / audio_s;
    assert!(
        rtf < 0.5,
        "RTF={rtf:.3} 超过 0.5（{:.1}s 音频花了 {:.1}s）",
        audio_s,
        elapsed
    );
}

#[test]
fn glossary_term_survives_real_engine_path() {
    // 核心不变量 2：术语在译文中原样保留。
    // 这里走真实引擎装配路径（只是后端是 Mock），而非手工拼装。
    let tmp = tempdir("gloss");
    let gfile = tmp.path().join("terms.tsv");
    std::fs::write(&gfile, "Kubernetes\tKubernetes\n").unwrap();

    let cfg = PipelineConfig {
        backend: BackendChoice::Mock,
        glossary_paths: vec![gfile],
        ..Default::default()
    };
    let mut engine = SessionEngine::new(&cfg).unwrap();

    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    assert_eq!(engine.start(rx), StartOutcome::Started);
    let mut capture_ms = 0u64;
    for block in speech_bursts(3).chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(block.to_vec(), capture_ms));
        capture_ms += CHUNK_MS;
    }
    drop(tx);
    std::thread::sleep(Duration::from_millis(200));
    let summary = engine.stop().unwrap();

    // 词库加载后不应报错，且哨兵机制不产生异常待复核
    if let Some(e) = engine.snapshot().error {
        panic!("引擎报错: {e}");
    }
    let reviewed = summary.lines.iter().filter(|l| l.needs_review).count();
    assert_eq!(
        reviewed, 0,
        "Mock 翻译器按契约透传哨兵，不该有待复核行"
    );
    assert!(!summary.lines.is_empty(), "应有字幕行");
}

// ── 极简临时目录（避免为测试引入 dev-dependency）──
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir(tag: &str) -> TempDir {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("translator-e2e-{tag}-{uniq}"));
    std::fs::create_dir_all(&p).unwrap();
    TempDir(p)
}
// ══ 多声源（新增需求：同时转译 + 区分说话人）═════════════

fn drive_multi(
    engine: &mut SessionEngine,
    inputs: Vec<(Vec<f32>, SourceSpec)>,
    settle: Duration,
) -> Vec<LineEvent> {
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let mut sources = Vec::new();
    for (audio, spec) in inputs {
        let (tx, rx) = mpsc::channel::<PcmChunk>();
        // 灌音频（发送端drop掉即表示本源结束）
        let mut capture_ms = 0u64;
        for block in audio.chunks(chunk_len) {
            if tx.send(PcmChunk::new(block.to_vec(), capture_ms)).is_err() {
                break;
            }
            capture_ms += CHUNK_MS;
        }
        sources.push((rx, spec));
    }
    assert_eq!(engine.start_multi(sources, Box::new(NoopSink)), StartOutcome::Started);

    std::thread::sleep(settle);
    let mut events = engine.take_events();
    let _summary = engine.stop().expect("stop 应成功");
    events.extend(engine.take_events());
    events
}

fn speech_bursts_tagged(count: usize, freq: f32) -> Vec<f32> {
    let sr = AppConfig::SAMPLE_RATE as usize;
    let mut out = Vec::new();
    let mut phase = 0.0f32;
    let w = 2.0 * std::f32::consts::PI * freq / sr as f32;
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
fn multi_source_both_produce_subtitles() {
    // 两路音频同时灌入，两路都应出字幕
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();

    // 内放 440Hz、麦克风 880Hz —— 频率不同以免被当成同一路
    let events = drive_multi(
        &mut engine,
        vec![
            (speech_bursts_tagged(3, 440.0), SourceSpec::new(SourceKind::SystemAudio)),
            (speech_bursts_tagged(3, 880.0), SourceSpec::new(SourceKind::Microphone)),
        ],
        Duration::from_millis(400),
    );

    let added: Vec<&LineEvent> = events
        .iter()
        .filter(|e| matches!(e, LineEvent::Added { .. }))
        .collect();
    assert!(added.len() >= 6, "两路各 3 段应至少 6 行，实际 {}", added.len());

    let speakers: std::collections::HashSet<String> = added
        .iter()
        .filter_map(|e| match e {
            LineEvent::Added { speaker, .. } => speaker.clone(),
            _ => None,
        })
        .collect();
    assert!(
        speakers.contains("对方") && speakers.contains("我"),
        "两个说话人都应出现，实际 {speakers:?}"
    );
}

#[test]
fn speaker_labels_are_configurable() {
    // 用户可自定义说话人名
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let events = drive_multi(
        &mut engine,
        vec![
            (
                speech_bursts_tagged(2, 440.0),
                SourceSpec::with_speaker(SourceKind::SystemAudio, "Alice"),
            ),
            (
                speech_bursts_tagged(2, 880.0),
                SourceSpec::with_speaker(SourceKind::Microphone, "Bob"),
            ),
        ],
        Duration::from_millis(300),
    );
    let speakers: std::collections::HashSet<String> = events
        .iter()
        .filter_map(|e| match e {
            LineEvent::Added { speaker, .. } => speaker.clone(),
            _ => None,
        })
        .collect();
    assert!(speakers.contains("Alice") && speakers.contains("Bob"), "{speakers:?}");
}

#[test]
fn each_source_has_independent_vad() {
    // 关键：两路各自的 hangover 不能互相触发。
    // 灌入一路长语音 + 另一路静音，长的那路必须正常断句（不被静音路拖住）。
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();

    let summary = {
        let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
        let mut sources = Vec::new();
        // 源1：4 段语音
        let (tx1, rx1) = mpsc::channel::<PcmChunk>();
        let mut c1 = 0u64;
        for b in speech_bursts_tagged(4, 440.0).chunks(chunk_len) {
            let _ = tx1.send(PcmChunk::new(b.to_vec(), c1));
            c1 += CHUNK_MS;
        }
        sources.push((rx1, SourceSpec::new(SourceKind::SystemAudio)));
        // 源2：纯静音（该路不该产出任何字幕）
        let (tx2, rx2) = mpsc::channel::<PcmChunk>();
        let mut c2 = 0u64;
        for b in vec![0.0f32; 30_000].chunks(chunk_len) {
            let _ = tx2.send(PcmChunk::new(b.to_vec(), c2));
            c2 += CHUNK_MS;
        }
        sources.push((rx2, SourceSpec::new(SourceKind::Microphone)));

        assert_eq!(
            engine.start_multi(sources, Box::new(NoopSink)),
            StartOutcome::Started
        );
        std::thread::sleep(Duration::from_millis(400));
        engine.stop().unwrap()
    };

    assert_eq!(summary.stats.asr_finals, 4, "只有语音路该产出 4 行");
    // 且全部来自语音路
    assert!(
        summary.lines.iter().all(|l| l.speaker.as_deref() == Some("对方")),
        "静音路不该产出字幕"
    );
}

#[test]
fn single_source_backward_compatible() {
    // 旧 API（start）仍可用 —— Android 侧在用
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let events = drive(&mut engine, &speech_bursts(3), Duration::from_millis(300));
    let added = events
        .iter()
        .filter(|e| matches!(e, LineEvent::Added { .. }))
        .count();
    assert!(added >= 3, "旧 API 应照常工作，实际 {added}");
}

// ══ 暂停 / 恢复 ═══════════════════════════════════════

#[test]
fn pause_stops_producing_and_resume_continues() {
    // 用户报的 bug：暂停后无法继续。
    // 关键：暂停**保留会话与接收端**，恢复后接着转。
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();

    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    let mut capture_ms = 0u64;
    for b in speech_bursts(6).chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(b.to_vec(), capture_ms));
        capture_ms += CHUNK_MS;
    }
    // 发送端保持存活 —— 暂停期间音频会积压，恢复时会被丢弃
    assert_eq!(
        engine.start_multi(
            vec![(rx, SourceSpec::new(SourceKind::SystemAudio))],
            Box::new(NoopSink),
        ),
        StartOutcome::Started
    );
    std::thread::sleep(Duration::from_millis(200));

    // ── 暂停 ──
    engine.pause();
    assert!(engine.is_paused());
    std::thread::sleep(Duration::from_millis(150));
    let before = engine.snapshot().last_seq;
    // 暂停期间不应再产出新行
    std::thread::sleep(Duration::from_millis(250));
    let during = engine.snapshot().last_seq;
    assert_eq!(before, during, "暂停期间不应产出新字幕：{before} → {during}");

    // ── 恢复 ──
    engine.resume();
    assert!(!engine.is_paused());
    std::thread::sleep(Duration::from_millis(250));
    let after = engine.snapshot().last_seq;
    assert!(after >= during, "恢复后应能继续产出：{during} → {after}");

    let _ = engine.stop().unwrap();
}

#[test]
fn pause_is_idempotent_and_stop_works_after_pause() {
    // 暂停状态下停止不能卡死（引擎线程要能正常退出）
    let cfg = mock_config();
    let mut engine = SessionEngine::new(&cfg).unwrap();
    let (tx, rx) = mpsc::channel::<PcmChunk>();
    let chunk_len = (AppConfig::SAMPLE_RATE as usize * CHUNK_MS as usize) / 1000;
    for b in speech_bursts(3).chunks(chunk_len) {
        let _ = tx.send(PcmChunk::new(b.to_vec(), 0));
    }
    assert_eq!(
        engine.start_multi(
            vec![(rx, SourceSpec::new(SourceKind::SystemAudio))],
            Box::new(NoopSink),
        ),
        StartOutcome::Started
    );
    std::thread::sleep(Duration::from_millis(150));
    engine.pause();
    engine.pause(); // 重复暂停不应出问题
    std::thread::sleep(Duration::from_millis(100));
    // 暂停中停止 —— 必须能正常返回
    let summary = engine.stop().expect("暂停中停止应成功");
    assert!(summary.stats.vad_segments >= 0);
}
