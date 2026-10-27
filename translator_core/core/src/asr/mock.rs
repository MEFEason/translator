//! 不依赖原生库的参考 ASR 实现。
//!
//! 两个用途：
//! 1. 在 sherpa-onnx 原生库尚未编译好时，**跑通整条管线**并验证时序。
//! 2. 作为黄金测试的对照基线 —— 三端一致性测试需要一个确定性输出源。
//!
//! 它不识别真实语音，而是按能量与时长产出**可预期的**分段，
//! 用于验证"VAD → ASR → 词库 → MT → 字幕"链路的时序与背压行为。

use super::{AsrBackend, AsrEvent, AsrSession, ChunkSplitter};
use crate::audio::PcmChunk;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 记录会话统计，便于验证背压与丢帧行为
#[derive(Debug, Default)]
pub struct MockStats {
    pub sessions_created: AtomicU64,
    pub samples_accepted: AtomicU64,
    pub finals_emitted: AtomicU64,
}

pub struct MockBackend {
    stats: Arc<MockStats>,
    /// 累积够多少样本就产出一个词
    samples_per_word: usize,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(MockStats::default()),
            // 每 0.4 秒（16k × 0.4 = 6400 样本）产出一个词
            samples_per_word: 6400,
        }
    }

    pub fn with_word_interval(samples_per_word: usize) -> Self {
        Self {
            stats: Arc::new(MockStats::default()),
            samples_per_word: samples_per_word.max(1),
        }
    }

    pub fn stats(&self) -> Arc<MockStats> {
        Arc::clone(&self.stats)
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AsrBackend for MockBackend {
    fn create_session(&self, hotwords: Vec<String>) -> crate::Result<Box<dyn AsrSession>> {
        self.stats.sessions_created.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MockSession {
            hotwords,
            splitter: ChunkSplitter::new(3200),
            accepted: 0,
            words_emitted: 0,
            last_finalize_accepted: 0,
            speech_samples: 0,
            finished: false,
            interval: self.samples_per_word,
            stats: Arc::clone(&self.stats),
        }))
    }

    fn name(&self) -> &'static str {
        "mock (测试用，非真实识别)"
    }

    fn is_available(&self) -> bool {
        true
    }
}

pub struct MockSession {
    hotwords: Vec<String>,
    splitter: ChunkSplitter,
    accepted: usize,
    words_emitted: usize,
    /// 上次 finalize 时已接受的**语音**样本数。
    ///
    /// 用于只交出「本次新增」的词 —— 对齐 sherpa 的 reset 语义。
    last_finalize_accepted: usize,
    /// 累计的**语音**样本数（能量高于阈值才算）。
    ///
    /// ## 为什么不能直接用总样本数
    ///
    /// 总样本数包含**静音**。引擎现在连续喂音频（含段间静音），
    /// 若按总样本产词，纯静音的声源也会被算成「有话可说」，
    /// 产出莫名其妙的字幕行。
    /// 真实 ASR 不会 —— 没有语音就没有文本。
    speech_samples: usize,
    finished: bool,
    interval: usize,
    stats: Arc<MockStats>,
}

impl MockSession {
    /// 构造可预期的假设文本。命中热词时用热词，否则用占位词。
    fn hypothesis(&self) -> String {
        if let Some(w) = self.hotwords.get(self.words_emitted) {
            return format!("{w} ");
        }
        format!("word{} ", self.words_emitted)
    }

    /// 当前 partial 事件（内部使用，不在 trait 上暴露）
    fn poll(&self) -> crate::Result<Vec<AsrEvent>> {
        if self.finished || self.accepted == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![AsrEvent::Partial {
            text: self.hypothesis(),
            timestamp_s: 0.0,
        }])
    }
}

impl AsrSession for MockSession {
    fn accept(&mut self, samples: &[f32]) -> crate::Result<Vec<AsrEvent>> {
        if self.finished {
            return Err(crate::Error::Internal("会话已结束".into()));
        }
        self.accepted += samples.len();
        self.stats
            .samples_accepted
            .fetch_add(samples.len() as u64, Ordering::Relaxed);

        // 只把**有能量**的样本算作语音 —— 静音不产词。
        // 阈值 0.01（-40 dBFS）：真实语音远高于此，纯静音远低于此。
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
        if rms >= 0.01 {
            self.speech_samples += samples.len();
        }

        let chunk = PcmChunk::new(samples.to_vec(), 0);
        self.splitter.push(&chunk);

        // 与真实引擎一致：accept 只产出 partial
        Ok(self.poll()?)
    }

    fn finalize_segment(&mut self) -> crate::Result<Vec<AsrEvent>> {
        // Mock 不做端点判定：上层要断句时才交出一句。
        //
        // ## 与 sherpa 的语义对齐
        //
        // sherpa 的 `finalize_segment` 交出「本句解码出的文本」，
        // 句间不重复、不丢失。这里用 `words_emitted` 记录已交出的词数，
        // 只交出**新增**的部分 —— 与 sherpa 行为一致。
        //
        // ⚠️ 不能靠 `accepted` 计数：`accepted` 含段间静音
        // （引擎现在连续喂音频），第一句就会把后面几句的词全吃掉。
        if self.finished {
            return Ok(Vec::new());
        }

        // **只交出上次 finalize 之后新增的词** —— 与 sherpa 的
        // `finalize_segment` 语义一致（它交出「上次 reset 以来」的文本）。
        //
        // ⚠️ 不能用累计样本数：那会把后面几句的词提前吃掉。
        let new_samples = self.speech_samples.saturating_sub(self.last_finalize_accepted);
        let new_words = new_samples / self.interval;
        if new_words == 0 {
            return Ok(Vec::new());
        }
        self.stats.finals_emitted.fetch_add(1, Ordering::Relaxed);

        let text = (0..new_words)
            .map(|_| {
                let i = self.words_emitted;
                self.words_emitted += 1;
                if let Some(w) = self.hotwords.get(i) {
                    w.clone()
                } else {
                    format!("word{i}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.last_finalize_accepted = self.speech_samples;

        Ok(vec![AsrEvent::Final {
            text,
            start_s: 0.0,
            end_s: 0.0,
            tokens: vec![],
        }])
    }

    fn finish(&mut self) -> crate::Result<Vec<AsrEvent>> {
        if self.finished {
            return Ok(Vec::new()); // 幂等
        }
        self.finished = true;
        // 与 finalize_segment 同语义：只交出尚未交出的词。
        //
        // ⚠️ 不能用 `.max(1)` 兜底 —— 那样即使没有新增内容也会产出一行，
        // 导致行数比 ASR 实际产出的多（收尾时多吐一句空行）。
        let new_samples = self.speech_samples.saturating_sub(self.last_finalize_accepted);
        let new_words = new_samples / self.interval;
        if new_words == 0 {
            return Ok(Vec::new());
        }
        self.stats.finals_emitted.fetch_add(1, Ordering::Relaxed);

        let text = (0..new_words)
            .map(|_| {
                let i = self.words_emitted;
                self.words_emitted += 1;
                if let Some(w) = self.hotwords.get(i) {
                    w.clone()
                } else {
                    format!("word{i}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.last_finalize_accepted = self.speech_samples;

        Ok(vec![AsrEvent::Final {
            text,
            start_s: 0.0,
            end_s: self.accepted as f32 / crate::config::AppConfig::SAMPLE_RATE as f32,
            tokens: vec![],
        }])
    }

    fn partial_text(&self) -> String {
        if self.accepted == 0 {
            String::new()
        } else {
            self.hypothesis()
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::resample;

    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * 0.05).sin() * 0.3).collect()
    }

    #[test]
    fn session_emits_final_after_finish() {
        let b = MockBackend::new();
        let mut s = b.create_session(vec![]).unwrap();
        s.accept(&tone(32_000)).unwrap(); // 2 秒 → 5 个词
        let ev = s.finish().unwrap();
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            AsrEvent::Final { text, .. } => {
                assert_eq!(text.split_whitespace().count(), 5);
            }
            other => panic!("期望 Final，得到 {other:?}"),
        }
    }

    #[test]
    fn empty_audio_produces_no_final() {
        let b = MockBackend::new();
        let mut s = b.create_session(vec![]).unwrap();
        assert!(s.finish().unwrap().is_empty());
    }

    #[test]
    fn hotwords_used_in_output() {
        let b = MockBackend::new();
        let mut s = b
            .create_session(vec!["Kubernetes".into(), "gRPC".into()])
            .unwrap();
        s.accept(&tone(32_000)).unwrap();
        let ev = s.finish().unwrap();
        match &ev[0] {
            AsrEvent::Final { text, .. } => {
                assert!(text.starts_with("Kubernetes gRPC"), "实际: {text}");
            }
            other => panic!("期望 Final，得到 {other:?}"),
        }
    }

    #[test]
    fn accept_returns_partial_while_active() {
        let b = MockBackend::new();
        let mut s = b.create_session(vec![]).unwrap();
        let ev = s.accept(&tone(3200)).unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], AsrEvent::Partial { .. }));
    }

    #[test]
    fn accept_after_finish_errors() {
        let b = MockBackend::new();
        let mut s = b.create_session(vec![]).unwrap();
        s.finish().unwrap();
        assert!(s.accept(&tone(100)).is_err());
    }

    #[test]
    fn finish_is_idempotent() {
        let b = MockBackend::new();
        let mut s = b.create_session(vec![]).unwrap();
        s.accept(&tone(16_000)).unwrap();
        assert_eq!(s.finish().unwrap().len(), 1);
        assert!(s.finish().unwrap().is_empty(), "重复 finish 应返回空");
    }

    #[test]
    fn stats_track_usage() {
        let b = MockBackend::new();
        let stats = b.stats();
        let mut s = b.create_session(vec![]).unwrap();
        s.accept(&tone(16_000)).unwrap();
        s.finish().unwrap();

        assert_eq!(stats.sessions_created.load(Ordering::Relaxed), 1);
        assert_eq!(stats.samples_accepted.load(Ordering::Relaxed), 16_000);
        assert_eq!(stats.finals_emitted.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn backend_is_send_and_sync() {
        fn assert_ss<T: Send + Sync>() {}
        assert_ss::<MockBackend>();
    }

    #[test]
    fn works_with_realistic_pipeline_sequence() {
        // 模拟真实链路：48k 重采样到 16k → 分块送 ASR → 收 final
        let b = MockBackend::new();
        let mut s = b.create_session(vec!["Kubernetes".into()]).unwrap();
        let chunk = PcmChunk::new(resample::resample_linear(&tone(48_000), 48_000, 16_000), 0);
        for block in chunk.samples.chunks(320) {
            let _ = s.accept(block).unwrap();
        }
        let ev = s.finish().unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], AsrEvent::Final { .. }));
    }
}
