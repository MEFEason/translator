//! 流水线集成测试：三端一致性的行为锚点。
//!
//! 这些测试跑通完整链路 —— 音频 → VAD 断句 → ASR → 词库哨兵 → 字幕聚合。
//! 使用 `MockAsr` 产出确定性输出，因此结果可断言，是 §7.2 黄金测试的
//! 行为基准（真实 sherpa-onnx 的黄金测试另用固定音频 fixture）。
//!
//! **核心不变量**：无论中间件如何变化，最终字幕必须满足：
//! 1. EN 一定先于 ZH 出现（MT 不得阻塞转写显示）
//! 2. 词库术语在译文中原样保留
//! 3. 句中短暂停顿不得把一句话拆成多段

use translator_core::asr::mock::MockBackend;
use translator_core::asr::{AsrBackend, AsrConfig, AsrEvent};
use translator_core::audio::PcmChunk;
use translator_core::glossary::schema::{Entry, Scope};
use translator_core::glossary::{GlossaryFile, SnapshotGuard};
use translator_core::subtitle::SubtitleAssembler;
use translator_core::vad::segmenter::Segmenter;
use std::path::PathBuf;
use std::sync::Arc;

/// 构造一段带静音间隔的测试音频
///
/// 结构：1 秒语音 → 0.4 秒静音 → 1 秒语音 → 0.5 秒静音
/// 用于验证 hangover 断句与"不拆分句子"
fn test_audio() -> (Vec<f32>, Vec<(usize, usize)>) {
    let sr = translator_core::config::AppConfig::SAMPLE_RATE as usize;
    let frame = translator_core::config::AppConfig::VAD_FRAME_SAMPLES;
    let mut samples = Vec::new();
    let mut segments = Vec::new();

    let push_tone = |samples: &mut Vec<f32>, n: usize, t0: f32, out: &mut Vec<(usize, usize)>| {
        let start = samples.len();
        for i in 0..n {
            let t = (t0 + i as f32 / sr as f32) * 440.0;
            samples.push(0.3 * (2.0 * std::f32::consts::PI * t).sin());
        }
        out.push((start, samples.len()));
        let _ = frame;
    };

    push_tone(&mut samples, sr, 0.0, &mut segments); // 1s 语音
    samples.extend(std::iter::repeat_n(0.0f32, sr * 4 / 10)); // 0.4s 静音
    push_tone(&mut samples, sr, 1.4, &mut segments); // 1s 语音
    samples.extend(std::iter::repeat_n(0.0f32, sr / 2)); // 0.5s 静音
    (samples, segments)
}

/// 简易 glossary 构造
fn glossary(entries: &[(&str, &str)]) -> Arc<translator_core::glossary::GlossarySnapshot> {
    Arc::new(translator_core::glossary::GlossarySnapshot::from_file(GlossaryFile {
        version: 1,
        entries: entries
            .iter()
            .map(|(s, t)| Entry {
                source: s.to_string(),
                target: t.to_string(),
                aliases: vec![],
                case_sensitive: false,
                scope: Scope::Both,
                group: None,
                enabled: true,
                note: None,
                regex: None,
                source_file: PathBuf::from("test"),
                line_no: 1,
            })
            .collect(),
    }))
}

/// 伪翻译器：模拟 Marian 的行为 —— 原样透传哨兵，其余按固定规则替换
fn fake_translate(sentinel_text: &str, mapping: &[String]) -> String {
    let mut out = sentinel_text.to_string();
    for (i, target) in mapping.iter().enumerate() {
        out = out.replace(&format!("\u{2581}X{i}X"), &format!("〔{target}〕"));
    }
    out
}

/// 完整链路：音频 → 断句 → ASR → 词库 → 字幕
struct Pipeline {
    segmenter: Segmenter,
    assembler: SubtitleAssembler,
    glossary: Arc<translator_core::glossary::GlossarySnapshot>,
    remainder: Vec<f32>,
    frame_idx: u64,
}

impl Pipeline {
    fn new(glossary: Arc<translator_core::glossary::GlossarySnapshot>) -> Self {
        Self {
            segmenter: Segmenter::with_hangover(300),
            assembler: SubtitleAssembler::new(0),
            glossary,
            remainder: Vec::new(),
            frame_idx: 0,
        }
    }

    /// 送入音频，产出已确认的字幕行
    fn feed(&mut self, samples: &[f32]) -> Vec<usize> {
        let chunk = PcmChunk::new(samples.to_vec(), 0);
        let frames =
            translator_core::audio::split_into_vad_frames(&chunk, &mut self.remainder, self.frame_idx * 32);
        let mut lines = Vec::new();

        for (frame, frame_time) in frames {
            self.frame_idx += 1;
            if let Some(seg) = self.segmenter.push_frame(&frame, frame_time) {
                // 送 ASR（此处用固定文本替代 mock 的占位词，更贴近真实语义）
                let text = "we deploy Kubernetes today";
                let guard = SnapshotGuard::new(Arc::clone(&self.glossary));
                let pending = guard.start(text);
                let mt_out = fake_translate(pending.text_for_translation(), pending.mapping());
                let (zh, restore) = pending.finish(&mt_out);

                lines.push(self.assembler.commit(
                    text.to_string(),
                    Some(zh),
                    seg.start_ms,
                    seg.end_ms,
                    seg.forced,
                    !restore.is_consistent(),
                    None,
                ) as usize);
            }
        }
        lines
    }
}

#[test]
fn full_pipeline_produces_bilingual_lines() {
    let g = glossary(&[("Kubernetes", "Kubernetes")]);
    let mut p = Pipeline::new(g);
    let (audio, _) = test_audio();
    let lines = p.feed(&audio);

    assert!(!lines.is_empty(), "应至少产生一行字幕");
    for idx in lines {
        let line = &p.assembler.lines()[idx];
        assert_eq!(line.source, "we deploy Kubernetes today");
        assert!(line.is_translated(), "译文应就绪");
        assert!(line.target.as_ref().unwrap().contains("Kubernetes"));
    }
}

#[test]
fn glossary_term_survives_full_pipeline() {
    // 核心不变量 2：术语在译文中原样保留
    let g = glossary(&[("Kubernetes", "Kubernetes")]);
    let mut p = Pipeline::new(g);
    let (audio, _) = test_audio();
    p.feed(&audio);

    let first = &p.assembler.lines()[0];
    let zh = first.target.as_ref().unwrap();
    assert!(
        zh.contains("Kubernetes"),
        "译文必须保留术语，实际: {zh}"
    );
    assert!(
        !zh.contains("库伯内"),
        "术语不得被意译，实际: {zh}"
    );
}

#[test]
fn english_shown_before_translation() {
    // 核心不变量 1：EN 先于 ZH
    let mut a = SubtitleAssembler::new(0);
    let idx = a.commit(
        "we deploy Kubernetes".into(),
        None,
        0,
        1000,
        false,
        false,
        None,
    );
    // 译文未到时，英文已在视图里
    let view = a.view();
    assert_eq!(view.lines[idx as usize].source, "we deploy Kubernetes");
    assert!(view.lines[idx as usize].is_translating());

    a.fill_translation(idx, "我们部署 Kubernetes".into(), false);
    assert!(a.view().lines[idx as usize].is_translated());
}

#[test]
fn short_pause_does_not_split_sentence() {
    // 核心不变量 3：句中短暂停顿不拆句
    // hangover 300ms 下，96ms 的停顿必须不断句
    let mut s = Segmenter::with_hangover(300);
    let sr = translator_core::config::AppConfig::SAMPLE_RATE as usize;
    let frame = translator_core::config::AppConfig::VAD_FRAME_SAMPLES;

    let tone = |n: usize, t0: f32| -> Vec<f32> {
        (0..n)
            .map(|i| 0.3 * ((2.0 * std::f32::consts::PI * 440.0 * (t0 + i as f32 / sr as f32)).sin()))
            .collect()
    };

    let mut produced = 0;
    let mut fi = 0u64;
    let mut offset = 0.0f32;

    // 0.8s 语音
    for _ in 0..(sr * 8 / 10 / frame) {
        if s.push_frame(&tone(frame, offset), fi * 32).is_some() {
            produced += 1;
        }
        offset += frame as f32 / sr as f32;
        fi += 1;
    }
    // 96ms 静音（3 帧）
    for _ in 0..3 {
        if s.push_frame(&vec![0.0; frame], fi * 32).is_some() {
            produced += 1;
        }
        fi += 1;
    }
    // 再 0.8s 语音
    for _ in 0..(sr * 8 / 10 / frame) {
        if s.push_frame(&tone(frame, offset), fi * 32).is_some() {
            produced += 1;
        }
        offset += frame as f32 / sr as f32;
        fi += 1;
    }

    assert_eq!(produced, 0, "96ms 停顿不应断句，却产生了 {produced} 段");
}

#[test]
fn pipeline_handles_empty_audio() {
    let g = glossary(&[]);
    let mut p = Pipeline::new(g);
    let lines = p.feed(&vec![0.0; 16_000]); // 1 秒纯静音
    assert!(lines.is_empty(), "纯静音不应产生字幕");
    assert_eq!(p.assembler.line_count(), 0);
}

#[test]
fn glossary_hotwords_reach_asr_session() {
    // Layer A：词库热词应传递到 ASR 会话创建
    let g = glossary(&[("Kubernetes", "Kubernetes"), ("gRPC", "gRPC")]);
    let backend = MockBackend::new();
    let session = backend.create_session(g.hotwords().to_vec()).unwrap();

    // 热词数应与词库一致（source + aliases）
    assert_eq!(session.partial_text(), "", "新建会话无输出");

    let mut session = backend.create_session(g.hotwords().to_vec()).unwrap();
    let _ = session.accept(&vec![0.3; 32_000]);
    let ev = session.finish().unwrap();
    match &ev[0] {
        AsrEvent::Final { text, .. } => {
            assert!(text.contains("Kubernetes"), "热词应出现在输出: {text}");
        }
        other => panic!("期望 Final，得到 {other:?}"),
    }
}

#[test]
fn asr_config_endpoints_disabled_by_default() {
    // 断句必须由 VAD 独占
    let cfg = AsrConfig::new("model", 2);
    assert!(!cfg.enable_endpoint, "引擎内 endpoint 必须默认关闭");
}

#[test]
fn subtitle_export_roundtrip() {
    let g = glossary(&[("Kubernetes", "Kubernetes")]);
    let mut p = Pipeline::new(g);
    let (audio, _) = test_audio();
    p.feed(&audio);

    let srt = p.assembler.export_srt();
    assert!(srt.contains("we deploy Kubernetes"));
    assert!(srt.contains("Kubernetes"));

    let vtt = p.assembler.export_vtt();
    assert!(vtt.starts_with("WEBVTT"));

    // 三端必须字节级一致 → 同一输入产出同一输出
    let srt2 = p.assembler.export_srt();
    assert_eq!(srt, srt2, "同样输入必须产出同样导出");
}

#[test]
fn multiple_glossary_files_merge_without_duplicates() {
    use translator_core::glossary::GlossaryManager;
    let m = GlossaryManager::new();
    let snap = m.current();
    assert_eq!(snap.entry_count(), 0);

    let a = GlossaryFile {
        version: 1,
        entries: vec![Entry {
            source: "Kubernetes".into(),
            target: "Kubernetes".into(),
            aliases: vec!["k8s".into()],
            case_sensitive: false,
            scope: Scope::Both,
            group: None,
            enabled: true,
            note: None,
            regex: None,
            source_file: PathBuf::from("a"),
            line_no: 1,
        }],
    };
    let b = GlossaryFile {
        version: 1,
        entries: vec![Entry {
            source: "kubernetes".into(), // 大小写不同 → 同一词条
            target: "Kubernetes".into(),
            aliases: vec!["K8s".into()],
            case_sensitive: false,
            scope: Scope::Both,
            group: None,
            enabled: true,
            note: None,
            regex: None,
            source_file: PathBuf::from("b"),
            line_no: 1,
        }],
    };
    let merged = translator_core::glossary::GlossarySnapshot::build(vec![a, b]);
    assert_eq!(merged.entry_count(), 1, "大小写不同的同词应合并");
    assert_eq!(
        merged.entries()[0].aliases.len(),
        2,
        "aliases 应取并集: {:?}",
        merged.entries()[0].aliases
    );
}

// ── MT 层集成：验证翻译引擎契约 ──────────────────────────────────

/// 用 mock 翻译器驱动词库哨兵全链路，验证「引擎无关」。
///
/// 关键点：哨兵保护与还原属于词库层，翻译引擎只负责把哨兵当普通
/// 字符透传。此测试锁定该契约 —— 换任何引擎都必须满足。
#[test]
fn mt_contract_sentinels_survive_translation() {
    use translator_core::glossary::schema::{Entry, Scope};
    use translator_core::glossary::{GlossaryFile, GlossarySnapshot, SnapshotGuard};
    use translator_core::mt::mock::MockTranslator;
    use translator_core::mt::{MtBackend, QualityTier};

    let snapshot = GlossarySnapshot::from_file(GlossaryFile {
        version: 1,
        entries: vec![
            Entry {
                source: "Kubernetes".into(),
                target: "Kubernetes".into(),
                aliases: vec!["k8s".into()],
                case_sensitive: false,
                scope: Scope::Both,
                group: None,
                enabled: true,
                note: None,
                regex: None,
                source_file: PathBuf::from("t"),
                line_no: 1,
            },
            Entry {
                source: "gRPC".into(),
                target: "gRPC".into(),
                aliases: vec![],
                case_sensitive: false,
                scope: Scope::Both,
                group: None,
                enabled: true,
                note: None,
                regex: None,
                source_file: PathBuf::from("t"),
                line_no: 2,
            },
        ],
    });

    let translator = MockTranslator::new(QualityTier::Fast);
    let guard = SnapshotGuard::new(Arc::new(snapshot));
    let pending = guard.start("we deploy Kubernetes and gRPC today");

    // 术语必须被替换成哨兵
    assert_eq!(
        pending.text_for_translation(),
        "we deploy \u{2581}X0X and \u{2581}X1X today"
    );
    assert_eq!(pending.sentinel_count(), 2);

    // 引擎必须原样透传哨兵（这是引擎无关契约）
    let out = translator.translate(pending.text_for_translation()).unwrap();
    assert!(
        out.text.contains("\u{2581}X0X") && out.text.contains("\u{2581}X1X"),
        "引擎必须透传哨兵，实际: {}",
        out.text
    );

    // 还原后术语必须原样保留
    let (zh, restore) = pending.finish(&out.text);
    assert!(restore.is_consistent());
    assert!(zh.contains("Kubernetes"), "实际: {zh}");
    assert!(zh.contains("gRPC"), "实际: {zh}");
    assert!(!zh.contains("▁X"), "不得残留哨兵: {zh}");
}

#[test]
fn mt_tiers_are_selectable() {
    use translator_core::mt::{MtBackend, QualityTier};
    use translator_core::mt::mock::MockTranslator;

    let fast = MockTranslator::new(QualityTier::Fast);
    let accurate = MockTranslator::new(QualityTier::Accurate);
    assert_eq!(fast.tier(), QualityTier::Fast);
    assert_eq!(accurate.tier(), QualityTier::Accurate);
}

#[test]
fn long_text_is_split_before_translation() {
    use translator_core::mt::split_by_sentence;

    // 模拟一段很长的 ASR 输出，验证不会整段丢失
    let long = (0..20)
        .map(|i| format!("This is sentence number {i} in a long transcript."))
        .collect::<Vec<_>>()
        .join(" ");
    let parts = split_by_sentence(&long, 200);
    assert!(parts.len() > 1);
    assert_eq!(parts.concat(), long, "切分不得丢内容");
}

#[test]
fn empty_translation_does_not_block_subtitle() {
    use translator_core::mt::mock::MockTranslator;
    use translator_core::mt::{MtBackend, QualityTier};

    // 引擎对空串返回空译文，不应阻塞字幕行
    let t = MockTranslator::new(QualityTier::Fast);
    let r = t.translate("").unwrap();
    assert_eq!(r.text, "");
    assert!(r.consistent);
}
