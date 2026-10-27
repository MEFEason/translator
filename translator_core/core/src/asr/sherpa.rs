//! sherpa-onnx 流式 Zipformer 后端。
//!
//! ## 接口依据（**以编译器实测为准**，勿凭 docs.rs 摘要页改）
//!
//! docs.rs 的 HTML 摘要页对本 crate 的签名渲染有误，实测（cargo 1.99 / sherpa-onnx 1.13）：
//!
//! ```text
//! OnlineRecognizer::create(&config)                       // 单值，非 &[..]
//! OnlineRecognizer::create_stream(&self) -> OnlineStream
//! OnlineRecognizer::create_stream_with_hotwords(&self, hotword: &str)
//! OnlineRecognizer::decode(&mut stream) / is_ready(&stream) / is_endpoint(&stream)
//! OnlineRecognizer::reset(&mut stream)
//! OnlineRecognizer::get_result(&stream) -> Option<RecognizerResult>
//! OnlineStream::accept_waveform(&mut self, sample_rate: i32, samples: &[f32])
//! OnlineStream::input_finished(&mut self)                 // 无返回值
//! RecognizerResult { text, tokens, timestamps, segment, start_time, is_final }
//! ```
//!
//! 注意与摘要页的三处差异：`create` 是单值引用、`create_stream_with_hotwords`
//! 收**单个** `&str`（非 `&[&str]`）、各方法收**单个** stream 引用（非切片）。
//!
//! ## 线程模型
//!
//! recognizer 用 `Arc` 共享（省内存：多会话复用同一份模型权重），
//! session 独占自己的 `OnlineStream`。

use super::{AsrBackend, AsrConfig, AsrEvent, AsrSession, ChunkSplitter};
use crate::error::{Error, Result};
use std::sync::Arc;

/// Zipformer 256 ms 模型的理想块长（16 kHz × 0.2 s）
const CHUNK_SAMPLES: usize = 3200;
/// 会话结束时补的尾部静音，冲刷解码器上下文
/// 段末补静音时长（ms），用来「逼」解码器吐出最后的字。
///
/// 2026-10-24 用户实测「中文丢字多」：x-asr 960ms 模型每 96 帧（960 ms）
/// 才解码一次，300 ms 的尾部静音**不够凑满一个解码块** —— 句尾那几个字
/// 永远出不来（中文在句尾，丢得最明显）。改 1000 ms 覆盖一个完整 chunk。
const TAIL_PAD_MS: u32 = 1000;

pub struct SherpaBackend {
    recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
    num_threads: usize,
    /// 热词偏置打分，创建 stream 时逐词附加
    hotwords_score: f32,
}

impl SherpaBackend {
    /// 从模型目录加载。
    ///
    /// 目录需含：`encoder*.onnx` / `decoder*.onnx` / `joiner*.onnx` / `tokens.txt`
    pub fn load(cfg: &AsrConfig) -> Result<Self> {
        // 目录校验收敛到 modelspec —— 启动自检在 feature 关闭时
        // 也要用同一套规则判定「模型是否就位」，避免两处规则漂移。
        let files = crate::modelspec::probe_asr_dir(&cfg.model_dir)?;

        let mut c = sherpa_onnx::OnlineRecognizerConfig::default();
        c.model_config.transducer.encoder = Some(files.encoder);
        c.model_config.transducer.decoder = Some(files.decoder);
        c.model_config.transducer.joiner = Some(files.joiner);
        c.model_config.tokens = Some(files.tokens);
        // 双语模型（cjkchar+bpe）必须声明建模单元并给 bpe 词表，
        // 否则中文 token 解不出来（sherpa 官方配置要求）。
        if let Some(bpe) = files.bpe_vocab {
            c.model_config.modeling_unit = Some("cjkchar+bpe".into());
            c.model_config.bpe_vocab = Some(bpe);
        }
        c.model_config.provider = Some(cfg.provider.clone());
        c.model_config.num_threads = cfg.num_threads as i32;
        c.model_config.debug = false;

        c.feat_config.sample_rate = crate::config::AppConfig::SAMPLE_RATE as i32;
        c.feat_config.feature_dim = 80;

        c.decoding_method = Some(cfg.decoding_method.clone());
        // ── endpoint 必须开启 ──
        //
        // 2026-10-23 实测（真实模型 + 真实语音）：endpoint 开启后
        // 产出文本达到上界的 104%，关闭则只有 29%。
        //
        // 原因：sherpa 的 endpoint 规则基于**解码器实际吐字情况**判断句末，
        // 比纯能量 VAD 准得多；而且它只在「该说的都说了」时才触发，
        // 不会把词拦腰截断。
        //
        // 官方默认（python-api-examples/speech-recognition-from-microphone-with-endpoint-detection.py）：
        //   rule1=2.4s rule2=1.2s rule3=300s
        // rule3=300 实质关闭该规则（句子不可能有 300 秒长）。
        c.enable_endpoint = cfg.enable_endpoint;
        c.rule1_min_trailing_silence = cfg.rule1_min_trailing_silence;
        c.rule2_min_trailing_silence = cfg.rule2_min_trailing_silence;
        c.rule3_min_utterance_length = cfg.rule3_min_utterance_length;
        c.hotwords_score = cfg.hotwords_score;

        let recognizer = sherpa_onnx::OnlineRecognizer::create(&c)
            .ok_or_else(|| Error::EngineInit("OnlineRecognizer::create 返回 None".into()))?;

        Ok(Self {
            recognizer: Arc::new(recognizer),
            num_threads: cfg.num_threads,
            hotwords_score: cfg.hotwords_score,
        })
    }

    pub fn recognizer(&self) -> Arc<sherpa_onnx::OnlineRecognizer> {
        Arc::clone(&self.recognizer)
    }

    pub fn num_threads(&self) -> usize {
        self.num_threads
    }
}

impl AsrBackend for SherpaBackend {
    fn create_session(&self, hotwords: Vec<String>) -> Result<Box<dyn AsrSession>> {
        // sherpa-onnx 的热词格式是**每行一条**「词 : 权重」（hotwords_file
        // 就是按行解析的；create_stream_with_hotwords 同样逐行处理）。
        // ⚠️ 2026-10-24 修复：旧代码用双空格把 174 条拼成**一行** →
        // EncodeHotwords 把整串当一个超长短语解析 →
        // 「Encode hotwords failed, skipping」——热词偏置**从未生效**，
        // 这是用户「识别准确度低」的直接原因之一。改为 \n 连接。
        // 用它而非 hotwords_file，好处是词库变更时新建会话即可，
        // 无需重建 recognizer（省 2~3 秒）。
        let stream = if hotwords.is_empty() {
            self.recognizer.create_stream()
        } else {
            let joined = hotwords
                .iter()
                .map(|w| format!("{w} : {}", self.hotwords_score))
                .collect::<Vec<_>>()
                .join("\n");
            self.recognizer.create_stream_with_hotwords(&joined)
        };

        Ok(Box::new(SherpaSession {
            recognizer: Arc::clone(&self.recognizer),
            stream,
            splitter: ChunkSplitter::new(CHUNK_SAMPLES),
            last_text: String::new(),
            finished: false,
            sample_rate: crate::config::AppConfig::SAMPLE_RATE as i32,
        }))
    }

    fn name(&self) -> &'static str {
        "sherpa-onnx streaming zipformer"
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// 单个识别会话。独占一条 `OnlineStream`。
pub struct SherpaSession {
    recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
    stream: sherpa_onnx::OnlineStream,
    splitter: ChunkSplitter,
    last_text: String,
    finished: bool,
    sample_rate: i32,
}

impl SherpaSession {
    /// 推进解码直到无更多可算，收集事件。
    ///
    /// ## endpoint 命中时的处理（2026-10-23 新增）
    ///
    /// sherpa 的 endpoint 规则判定「这一句说完了」时：
    /// 1. 先把当前结果作为 `Final` 产出（**必须在 reset 之前**，
    ///    reset 后 `get_result` 会被清空）
    /// 2. 再 `reset(&stream)` —— 官方 `Reset()` **不重置特征提取器**，
    ///    只推进 `start_frame_index_`，所以时间戳仍全局连续，
    ///    而解码器状态被清空、准备接收下一句。
    fn drain(&mut self) -> Result<Vec<AsrEvent>> {
        let mut events = Vec::new();
        // 防御性上限：避免引擎异常时死循环
        let mut guard = 0;

        while self.recognizer.is_ready(&self.stream) {
            self.recognizer.decode(&self.stream);
            guard += 1;
            if guard > 4096 {
                return Err(Error::Engine("解码步数异常，已中止".into()));
            }

            if let Some(r) = self.recognizer.get_result(&self.stream) {
                if r.text != self.last_text {
                    self.last_text = r.text.clone();
                    events.push(AsrEvent::Partial {
                        text: r.text,
                        timestamp_s: r.timestamps.and_then(|t| t.first().copied()).unwrap_or(0.0),
                    });
                }
            }

            // endpoint 命中 → 这一句定型，交出 Final 并重置解码状态
            if self.recognizer.is_endpoint(&self.stream) {
                if let Some(ev) = self.take_current_final() {
                    events.push(ev);
                }
                self.recognizer.reset(&self.stream);
                self.last_text.clear();
            }
        }
        Ok(events)
    }

    /// 把当前解码结果转成 `Final` 事件（文本为空则返回 None）。
    fn take_current_final(&self) -> Option<AsrEvent> {
        let r = self.recognizer.get_result(&self.stream)?;
        let text = r.text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        Some(AsrEvent::Final {
            text,
            start_s: r.start_time.unwrap_or(0.0),
            end_s: r
                .timestamps
                .as_ref()
                .and_then(|t| t.last().copied())
                .unwrap_or(0.0),
            tokens: r.tokens,
        })
    }
}


impl AsrSession for SherpaSession {
    fn accept(&mut self, samples: &[f32]) -> Result<Vec<AsrEvent>> {
        if self.finished {
            return Err(Error::Internal("会话已结束，无法再送入音频".into()));
        }
        let chunk = crate::audio::PcmChunk::new(samples.to_vec(), 0);
        for block in self.splitter.push(&chunk) {
            self.stream.accept_waveform(self.sample_rate, &block);
        }

        // `drain` 会同时产出 Partial（预览）与 Final（endpoint 命中，句子定型）。
        // 上层按事件类型分发：Partial 上预览、Final 上正式字幕。
        Ok(self.drain()?)
    }

    fn finalize_segment(&mut self) -> Result<Vec<AsrEvent>> {
        // 冲刷 splitter 残留，让最后一点音频进入解码器
        if let Some(rest) = self.splitter.flush() {
            self.stream.accept_waveform(self.sample_rate, &rest);
        }
        // 补一段尾部静音，逼解码器吐出最后的字
        let pad_len =
            (self.sample_rate as f32 * (TAIL_PAD_MS as f32 / 1000.0)).round() as usize;
        self.stream.accept_waveform(self.sample_rate, &vec![0.0f32; pad_len]);

        // ⚠️ 不能丢弃 drain 的返回值（2026-10-24 定位：60% 的段被判空）。
        // drain 内部在 endpoint 命中时会**先取 Final 再 reset**。旧代码写成
        // `let _ = self.drain()?;` 把结果扔了，然后再 `take_current_final()` ——
        // 那是在 reset **之后**取，结果必然是空 → asr_empty 飙升、丢字。
        // 正确顺序：用 drain 给的 Final；drain 没给（endpoint 未命中）才自己取一次。
        let mut events = self.drain()?;
        if !events.iter().any(|e| matches!(e, AsrEvent::Final { .. })) {
            if let Some(ev) = self.take_current_final() {
                events.push(ev);
            }
        }
        // 重置解码状态准备下一句（特征提取器不重置 → 上下文与时间戳连续）
        self.recognizer.reset(&self.stream);
        self.last_text.clear();
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<AsrEvent>> {
        if self.finished {
            return Ok(Vec::new()); // 幂等
        }
        self.finished = true;

        // ⚠️ 顺序很重要：**先冲刷真实音频，再补静音**。
        //
        // 反过来（先静音后音频）会让 splitter 里残留的真实语音被 300 ms 静音
        // 隔在中间 —— 解码器看到的是「静音 → 语音 → 结束」，
        // 那段语音就成了孤立片段，往往解不出字。
        if let Some(rest) = self.splitter.flush() {
            self.stream.accept_waveform(self.sample_rate, &rest);
        }

        // 再补尾部静音，让解码器吐出最后的内容
        let pad_len = (self.sample_rate as f32 * (TAIL_PAD_MS as f32 / 1000.0)).round() as usize;
        let pad = vec![0.0f32; pad_len];
        self.stream.accept_waveform(self.sample_rate, &pad);

        self.stream.input_finished();
        let _ = self.drain()?;

        // 用最终结果产出 Final（携带 tokens，供后续对齐与导出用）
        let mut events = Vec::new();
        if let Some(r) = self.recognizer.get_result(&self.stream) {
            let text = r.text.trim().to_string();
            if !text.is_empty() {
                events.push(AsrEvent::Final {
                    text,
                    start_s: r.start_time.unwrap_or(0.0),
                    end_s: r
                        .timestamps
                        .as_ref()
                        .and_then(|t| t.last().copied())
                        .unwrap_or(0.0),
                    tokens: r.tokens,
                });
            }
        }
        Ok(events)
    }

    fn partial_text(&self) -> String {
        self.last_text.clone()
    }
}
