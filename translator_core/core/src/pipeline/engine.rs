//! 会话引擎：把六个零件装配成一条流水线。
//!
//! 线程职责（详见 [`crate::pipeline`] 模块文档）：
//!
//! | 线程 | 职责 | 绝不能做的事 |
//! |---|---|---|
//! | 引擎 | 断句 → ASR → commit EN → 派翻译 → 应用译文 | **绝不做 MT**（阻塞转写显示） |
//! | MT | 翻译 + 哨兵还原 | 不碰音频、不碰字幕聚合 |
//! | 采集（外部） | 产音频块 | 绝不做 ASR/MT/写盘 |
//!
//! ## 单写者原则
//!
//! `SubtitleAssembler` **只被引擎线程写**。UI 通过
//! [`SessionEngine::snapshot`] 读取已发布的快照副本，从不触碰内部状态。
//! 这样就不需要「边转写边导出」时的读锁，也就不会在实时路径上引入
//! 锁竞争（而锁竞争在音频链路上就是掉帧的来源）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::asr::{AsrBackend, AsrEvent, AsrSession};
use crate::audio::{agc::Agc, PcmChunk, split_into_vad_frames};
use crate::error::{Error, Result};
use crate::glossary::{GlossaryManager, PendingSegment};
use crate::mt::SharedMt;
use crate::subtitle::{SubtitleAssembler, SubtitleLine};
use crate::vad::segmenter::{Segmenter, SpeechSegment};

use super::live::LiveConfig;
use super::{LineEvent, NoopSink, PipelineConfig, SourceKind, SourceSpec, TranscriptSink};

/// MT 队列深度。
///
/// 有界而非无界：翻译卡住时**丢译文**（用户看到原文），
/// 好过内存无限增长（用户看到程序崩掉）。
const MT_QUEUE_DEPTH: usize = 32;

/// 停止时等待在途译文的总预算。超时即放弃 —— 用户点了停止就该立刻停。
const MT_FLUSH_BUDGET: Duration = Duration::from_secs(5);

/// 音频接收超时。短间隔才能让 stop 及时响应。
const AUDIO_POLL: Duration = Duration::from_millis(50);

/// 统计
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineStats {
    /// VAD 切出的段数
    pub vad_segments: u64,
    /// ASR 产出的非空 final 段数（= 正式字幕行数）
    pub asr_finals: u64,
    /// 已回填的译文数
    pub mt_done: u64,
    /// 因 MT 队列满而丢弃的译文数。
    ///
    /// 译文可以丢，EN 行不行 —— 所以这个数应当恒接近 0，
    /// 非零说明翻译线程跟不上（模型太大或线程太少）。
    pub mt_dropped: u64,
    /// 翻译失败次数
    pub mt_failed: u64,
    /// ASR 喂音频 / 取句时的错误次数。
    ///
    /// 持续增长说明音频链路有问题（采集格式不对 / 会话已终结）。
    pub asr_errors: u64,
}

/// 会话结束后的汇总。`stop()` 返回。
#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// 内存窗口内残留的行（更早的行已被丢弃，转写盘仍保留）
    pub lines: Vec<SubtitleLine>,
    pub srt: String,
    pub vtt: String,
    pub txt: String,
    pub stats: PipelineStats,
    /// 写入转写盘的路径（未落盘为 None）
    pub transcript_path: Option<String>,
    pub transcript_written: u64,
    pub transcript_failures: u64,
}

/// 会话状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 已创建，未启动
    Created = 0,
    /// 运行中
    Running = 1,
    /// 已停止
    Stopped = 2,
    /// 出错（error 字段有详情）
    Error = 3,
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState::Created
    }
}

/// 引擎线程发布给 UI 的只读快照
#[derive(Debug, Clone, Default)]
pub struct PublishedState {
    pub state: SessionState,
    pub last_seq: u64,
    pub partial: Option<String>,
    /// **流式译文**（2026-10-24 用户需求：翻译也流式）。
    /// 与 partial 同步生长：ASR 预览每前进一点，这里就重译一次当前
    /// 预览文本（bergamot 无增量 KV cache，做的是「全量重译最新文本」，
    /// 但对用户就是译文跟着变）。成行后清空。
    pub partial_zh: Option<String>,
    pub stats: PipelineStats,
    /// 致命错误（如模型加载失败），非空即需 UI 如实展示
    pub error: Option<String>,
    /// MT（翻译）后端是否真的可用。
    ///
    /// 阶段1（只装 ASR 模型）时为 false —— UI 据此明示
    /// 「翻译引擎未启用」，**禁止静默**（DESIGN §2.5.2）。
    /// 另附不可用原因，便于 UI 直接展示。
    pub mt_available: bool,
    /// MT 不可用原因（空 = 可用）
    pub mt_unavailable_reason: String,
}

/// `start()` 的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    /// 已在运行
    AlreadyRunning,
}

/// 一个 MT 任务。携带 `PendingSegment` 而非裸索引 ——
/// 哨兵 `▁X{n}X` 里的 n 是**快照内局部索引**，
/// 若只传文本、等 MT 回来再查新快照，术语会张冠李戴。
struct MtJob {
    seq: u64,
    pending: PendingSegment,
    /// 入队时刻 —— mt_loop 出译文时打印「入队→出译文」总耗时
    /// （含排队）。这是区分「成行等待慢」与「翻译推理慢」的关键数据，
    /// 2026-10-24 用户反馈翻译 10 秒不出来，必须先可测再调参。
    enqueued_at: Instant,
}

struct MtDone {
    seq: u64,
    target: String,
    needs_review: bool,
}

/// 会话引擎
pub struct SessionEngine {
    shared: Arc<Shared>,
    stop_flag: Arc<AtomicBool>,
    ev_rx: Option<Receiver<LineEvent>>,
    engine_thread: Option<JoinHandle<SessionSummary>>,
    mt_thread: Option<JoinHandle<()>>,
    /// MT 线程句柄槽：监督线程建好 MT 线程后塞进来。
    ///
    /// ## 为什么不直接等（2026-10-24 用户实测「点开始转写 UI 像卡住」）
    /// 旧实现 `mt_handle_rx.recv_timeout(30s)` —— 监督线程要加载 ASR+MT
    /// 模型（数秒），FFI 线程（= UI isolate）在这几秒里完全冻结。
    /// 改成槽：start_multi **立即返回**，句柄晚点由监督线程放入；
    /// stop 时从槽里取出来 join（取不到就说明 MT 还没建，无需 join）。
    /// MT 线程 + 流式译文线程的句柄槽（监督线程建好后塞入，stop 取出 join）。
    /// 用 Vec：除主 MT 线程外，0.9.23 起还有 partial 流式翻译线程。
    mt_handle_slot: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    transcript_path: Option<String>,
    /// 整份配置。start() 在监督线程里按它构建后端 ——
    /// 保留完整 cfg（而不是只留 profile/window 两个字段），
    /// 是为了让「构建后端」与「校验模型目录」用同一份真相。
    cfg: PipelineConfig,
    /// 暂停标志。暂停时引擎不消费音频、不识别，但保留接收端与在途译文。
    paused: Arc<AtomicBool>,
    /// 运行时可调参数（UI 拖动 Slider 即时生效，无需重启会话）。
    /// 与 `shared.live` 是同一个 Arc。
    live: Arc<LiveConfig>,
}

/// 已构建的后端
struct Backends {
    asr: Arc<dyn AsrBackend>,
    mt: SharedMt,
}

struct Shared {
    glossary: GlossaryManager,
    /// 预置后端（测试注入用）。为 None 时在监督线程里按配置构建 ——
    /// 真实引擎加载要数秒，**绝不能发生在 FFI 调用线程上**。
    backends: Option<Backends>,
    published: Mutex<PublishedState>,
    /// 运行时可调参数。FFI `translator_session_update_live` 写它，
    /// 引擎循环读它 —— 两者共享同一个 Arc。
    live: Arc<LiveConfig>,
}

impl SessionEngine {
    /// 按配置创建引擎。
    ///
    /// 只做「装配」与「模型目录校验」，**不加载引擎**：
    /// onnxruntime / Bergamot 初始化要几百毫秒到数秒，
    /// 不能阻塞 UI 线程。真正的加载在 [`Self::start`] 的引擎线程内完成。
    pub fn new(cfg: &PipelineConfig) -> Result<Self> {
        if cfg.backend == super::BackendChoice::Auto {
            // 只校验目录结构（毫秒级），**不加载引擎**
            crate::alog!("SessionEngine::new：校验目录结构 asr={}", cfg.asr_model_dir.display());
            cfg.validate()?;
            crate::alog!("SessionEngine::new：目录校验通过");
        }
        Self::assemble(cfg, None)
    }

    /// 注入自定义后端。
    ///
    /// 测试用它塞入「慢速 Mock 翻译器」来验证 MT 背压 ——
    /// 与其把 `latency_ms` 这种只为测试存在的旋钮塞进 `PipelineConfig`，
    /// 不如让依赖本身可替换。生产路径走 [`Self::new`]。
    pub fn with_backends(
        cfg: &PipelineConfig,
        asr: Arc<dyn AsrBackend>,
        mt: SharedMt,
    ) -> Result<Self> {
        Self::assemble(cfg, Some(Backends { asr, mt }))
    }

    fn assemble(cfg: &PipelineConfig, backends: Option<Backends>) -> Result<Self> {
        crate::alog!("assemble 开始：加载词库（{} 个文件）", cfg.glossary_paths.len());
        let glossary = GlossaryManager::new();
        if !cfg.glossary_paths.is_empty() {
            glossary
                .load_files(&cfg.glossary_paths)
                .map_err(|e| Error::GlossaryLoad(e.to_string()))?;
        }
        crate::alog!("assemble：词库就绪");

        let live = Arc::new(LiveConfig::from_config(
            cfg.asr_threads,
            cfg.profile.hangover_ms(),
            cfg.offline_refine,
        ));

        // MT 可用性**预判**（会话未启动时 UI 也要能如实显示）。
        //
        // 判据：MT 权重目录存在且非空 → 预期可用；否则不可用。
        // 真实可用性在引擎启动后由后端 `is_available()` 覆盖
        // （模型损坏等运行时问题）。
        let mt_expected = !cfg.mt_model_dir.as_os_str().is_empty()
            && cfg.mt_model_dir.is_dir();
        let mt_reason = if mt_expected {
            String::new()
        } else if cfg.mt_optional {
            "未安装翻译权重（仅识别模式）".to_string()
        } else {
            String::new()
        };

        Ok(Self {
            shared: Arc::new(Shared {
                glossary,
                backends,
                published: Mutex::new(PublishedState {
                    mt_available: mt_expected,
                    mt_unavailable_reason: mt_reason,
                    ..Default::default()
                }),
                live: Arc::clone(&live),
            }),
            stop_flag: Arc::new(AtomicBool::new(false)),
            ev_rx: None,
            engine_thread: None,
            mt_thread: None,
            mt_handle_slot: Arc::new(std::sync::Mutex::new(Vec::new())),
            transcript_path: None,
            cfg: cfg.clone(),
            paused: Arc::new(AtomicBool::new(false)),
            live,
        })
    }

    /// 热重载词库。失败保留旧快照（不影响正在转写的会话）。
    pub fn reload_glossary(&self) -> Result<()> {
        self.shared.glossary.reload()
    }

    /// 追加导入一个词库文件，返回该文件的有效词条数。
    pub fn load_glossary_file(&self, path: &std::path::PathBuf) -> Result<usize> {
        self.shared.glossary.add_file(path)
    }

    /// 当前词库的词条数
    pub fn glossary_entry_count(&self) -> usize {
        self.shared.glossary.entry_count()
    }

    /// 本次会话的配置。FFI 层用它做自检报告。
    pub fn config(&self) -> &PipelineConfig {
        &self.cfg
    }

    /// 运行时可调参数（2026-10-25）。
    ///
    /// UI 拖动 Slider 后经 FFI `translator_session_update_live` 写入，
    /// 引擎循环每轮读取 —— **转写不中断、会话不重建**。
    pub fn live(&self) -> Arc<LiveConfig> {
        Arc::clone(&self.live)
    }

    /// 启动引擎线程与 MT 线程。
    ///
    /// `audio_rx` 的发送方由采集层持有 —— 本函数**不接管**它，
    /// 所以停止时以 stop flag 为准，而非等通道断开。
    pub fn start(&mut self, audio_rx: Receiver<PcmChunk>) -> StartOutcome {
        let spec = SourceSpec::new(SourceKind::SystemAudio);
        self.start_multi(vec![(audio_rx, spec)], Box::new(NoopSink))
    }

    /// 带转写盘启动。
    pub fn start_with_sink(
        &mut self,
        audio_rx: Receiver<PcmChunk>,
        sink: Box<dyn TranscriptSink>,
    ) -> StartOutcome {
        let spec = SourceSpec::new(SourceKind::SystemAudio);
        self.start_multi(vec![(audio_rx, spec)], sink)
    }

    /// 多声源启动。
    ///
    /// 每个 `(rx, spec)` 一路音频。**每路有独立的 VAD 状态机**，
    /// 否则两路混在一起永远断不了句（见 [`AudioSource`]）。
    pub fn start_multi(
        &mut self,
        sources: Vec<(Receiver<PcmChunk>, SourceSpec)>,
        sink: Box<dyn TranscriptSink>,
    ) -> StartOutcome {
        if self.engine_thread.is_some() {
            return StartOutcome::AlreadyRunning;
        }
        self.stop_flag.store(false, Ordering::Relaxed);

        let (ev_tx, ev_rx) = mpsc::channel::<LineEvent>();
        let (mt_tx, mt_rx) = mpsc::sync_channel::<MtJob>(MT_QUEUE_DEPTH);
        let (done_tx, done_rx) = mpsc::channel::<MtDone>();

        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop_flag);
        let cfg = self.cfg.clone();
        // MT 线程句柄经**共享槽**回传（不再用通道等待）：监督线程建好 MT
        // 线程后塞进槽，start_multi **立即返回**。旧实现 recv_timeout 会在
        // UI 线程上阻塞到模型加载完成（数秒），用户看起来就是点了开始就卡住。
        let mt_handle_slot = Arc::clone(&self.mt_handle_slot);
        let paused = Arc::clone(&self.paused);
        // 新一轮开始时解除暂停
        paused.store(false, Ordering::Relaxed);

        let supervisor = std::thread::Builder::new()
            .name("pipeline-supervisor".into())
            .spawn(move || -> SessionSummary {
                // ★ 引擎加载在这里发生 —— onnxruntime 初始化要数秒，
                //   绝不能发生在 FFI 调用线程（= UI 线程）上。
                eprintln!("[supervisor] 开始加载后端（ASR+MT+词库）...");
                let backends = match resolve_backends(&shared, &cfg) {
                    Ok(b) => b,
                    Err(e) => {
                        set_shared_error(&shared, format!("引擎加载失败: {e}"));
                        set_shared_state(&shared, SessionState::Error);
                        // 引擎线程没起来，MT 线程也不会有 —— 用空句柄交差
                        // 引擎没起来，MT 无需 join（槽留空）
                        return empty_summary();
                    }
                };

                // 加载期间用户可能已经点了停止 —— 早退，别白等几秒
                if stop.load(Ordering::Relaxed) {
                    // 引擎没起来，MT 无需 join（槽留空）
                    return empty_summary();
                }

                // MT 线程在监督线程里创建：后端已经就绪，直接搬过去
                let mt_backend = backends.mt;
                // MT 可用性如实上报（阶段1 只装 ASR → false，
                // UI 据此明示「翻译引擎未启用」，禁止静默）
                {
                    let available = mt_backend.is_available();
                    let reason = mt_backend.unavailable_reason().unwrap_or_default();
                    if let Ok(mut p) = shared.published.lock() {
                        p.mt_available = available;
                        p.mt_unavailable_reason = reason;
                    }
                }
                // 流式译文线程共用同一个 MT 后端（SharedMt 是 Arc，clone 廉价）
                let partial_mt = Arc::clone(&mt_backend);
                let mt_handle = std::thread::Builder::new()
                    .name("pipeline-mt".into())
                    .spawn(move || mt_loop(mt_rx, mt_backend, done_tx));
                if let Ok(h) = mt_handle {
                    if let Ok(mut g) = mt_handle_slot.lock() {
                        g.push(h);
                    }
                }

                // ── 流式译文线程（2026-10-24 用户：翻译也要流式）──
                // 独立线程，**轮询**共享快照的 partial（成行后清空）：
                // partial 每前进一点就重译一次当前最新文本，写进
                // partial_zh。不排队（翻译慢时自动跳过旧预览，只翻最新），
                // 与主 MT 线程分开 —— 主 MT 队列要等成行才有任务，
                // 而流式译文必须在 partial 阶段就跑起来。
                if let Ok(ph) = std::thread::Builder::new()
                    .name("pipeline-partial-mt".into())
                    .spawn({
                        let shared = Arc::clone(&shared);
                        let stop = Arc::clone(&stop);
                        move || partial_mt_loop(shared, partial_mt, stop)
                    })
                {
                    if let Ok(mut g) = mt_handle_slot.lock() {
                        g.push(ph);
                    }
                } else {
                    // 引擎没起来，MT 无需 join（槽留空）
                }

                eprintln!("[supervisor] 后端就绪，MT 线程已建，进入 engine_loop");
                set_shared_state(&shared, SessionState::Running);
                // 长生命周期 ASR 会话：**每个声源一个**，整个会话复用。
                //
                // 热词在会话创建时固定（词库热更新需重建会话才生效）——
                // 这是有意的取舍：重建会话会丢左上下文，
                // 实测文本产出从 100% 掉到 29%。
                let hotwords = shared.glossary.current().hotwords().to_vec();
                let audio_sources: Vec<AudioSource> = sources
                    .into_iter()
                    .map(|(rx, spec)| {
                        // 会话创建失败不致命：引擎循环里会重试并上报错误。
                        let asr_session = backends.asr.create_session(hotwords.clone()).ok();
                        if asr_session.is_none() {
                            set_shared_error(
                                &shared,
                                "识别会话创建失败：ASR 后端不可用".to_string(),
                            );
                        }
                        AudioSource {
                            rx,
                            segmenter: Segmenter::new(cfg.profile),
                            remainder: Vec::new(),
                            kind: spec.kind,
                            agc: if matches!(spec.kind, SourceKind::Microphone) {
                                Some(Agc::new())
                            } else {
                                None
                            },
                            speaker: spec.speaker,
                            base_ms: None,
                            asr_session,
                            asr_fed_samples: 0,
                            last_partial: String::new(),
                        }
                    })
                    .collect();
                engine_loop(EngineCtx {
                    // live 必须在 shared 之前取（shared 随后被 move 进结构体）
                    live: Arc::clone(&shared.live),
                    shared,
                    stop,
                    asr: backends.asr,
                    paused,
                    sources: audio_sources,
                    mt_tx,
                    done_rx,
                    ev_tx,
                    sink,
                    memory_window_lines: cfg.memory_window_lines,
                    // 两遍解码第二遍：离线精修器（None = 单遍流式）
                    refiner: cfg.offline_refiner(),
                    asr_config: cfg.asr_config(),
                    applied_threads: cfg.asr_threads,
                    hotwords: hotwords.clone(),
                })
            });

        let supervisor = match supervisor {
            Ok(h) => h,
            Err(e) => {
                self.set_error(format!("监督线程创建失败: {e}"));
                return StartOutcome::Started;
            }
        };

        // ⚠️ recv 会无限阻塞 FFI 线程（= UI 线程）：后端加载慢时整个
        // 界面未响应（用户实测截图）。改 30s 超时兜底 —— 超时后 MT
        // 线程仍在后台跑（监督线程已 spawn），只是没人 join 它；
        // 用『界面卡死』换『句柄没人 join』是明确划得来的交易。
        // 不等待：句柄由监督线程稍后放入 mt_handle_slot（见上），
        // stop 时再从槽里取出 join。UI 线程零阻塞。
        self.mt_thread = None;
        self.engine_thread = Some(supervisor);
        self.ev_rx = Some(ev_rx);
        StartOutcome::Started
    }

    /// 暂停转写。
    ///
    /// 与 [`Self::stop`] 的区别：暂停**保留会话与音频接收端**，
    /// 恢复后接着转。用于「临时不想录」的场景。
    ///
    /// 暂停期间引擎不消费音频（接收端里会积压，恢复时丢弃），
    /// 但**在途译文继续处理** —— 不会出现"暂停很久后译文一次性冒出来"。
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    /// 恢复转写。
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// 取走自上次调用以来的增量字幕事件。
    ///
    /// 必须增量：全量每 100ms 序列化一次会直接把 UI 卡死。
    pub fn take_events(&mut self) -> Vec<LineEvent> {
        let mut out = Vec::new();
        if let Some(rx) = &self.ev_rx {
            while let Ok(e) = rx.try_recv() {
                out.push(e);
            }
        }
        out
    }

    /// 读取发布快照
    pub fn snapshot(&self) -> PublishedState {
        self.shared
            .published
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default()
    }

    /// 写入一条内部错误（供 start 失败等早期路径使用）
    pub fn set_error(&self, msg: String) {
        if let Ok(mut p) = self.shared.published.lock() {
            p.error = Some(msg);
        }
    }

    /// 停止并冲刷。会 join 两个线程，因此**耗时可达数秒**
    /// （在途译文要等完或超预算）。UI 必须在调用前显示「正在收尾」。
    pub fn stop(&mut self) -> Result<SessionSummary> {
        self.stop_flag.store(true, Ordering::Relaxed);

        let summary = match self.engine_thread.take() {
            Some(h) => h.join().map_err(|_| Error::Internal("引擎线程 panic".into()))?,
            None => {
                return Ok(SessionSummary {
                    lines: Vec::new(),
                    srt: String::new(),
                    vtt: String::new(),
                    txt: String::new(),
                    stats: PipelineStats::default(),
                    transcript_path: self.transcript_path.clone(),
                    transcript_written: 0,
                    transcript_failures: 0,
                })
            }
        };

        // MT 线程 + 流式译文线程靠通道关闭/stop 标志自行退出；句柄都
        // 攒在 mt_handle_slot（Vec）。全取出 join —— 取不到说明监督线程
        // 还没建完（引擎没起来），无需 join。
        let handles: Vec<JoinHandle<()>> = self
            .mt_handle_slot
            .lock()
            .map(|mut g| g.drain(..).collect())
            .unwrap_or_default();
        for h in handles {
            let _ = h.join();
        }

        Ok(summary)
    }

    /// 是否仍在运行
    pub fn is_running(&self) -> bool {
        self.engine_thread.is_some()
    }
}

impl Drop for SessionEngine {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.engine_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.mt_thread.take() {
            let _ = h.join();
        }
    }
}

/// 一个声源在引擎线程里的状态。
///
/// ## 为什么每个源要有独立的 Segmenter 与 remainder
///
/// **不能共用**。两路音频（内放 + 麦克风）混进同一个 VAD 状态机时：
/// - 对方说话时 hangover 持续被触发，永远断不了句
/// - 自己的声音会被当成"句中短暂停顿"吃掉
///
/// 独立状态机才能让两路各自断句，再靠 seq 归并到同一条时间轴上。
struct AudioSource {
    rx: Receiver<PcmChunk>,
    segmenter: Segmenter,
    /// VAD 帧对齐用的残余样本（每源独立，帧边界不能跨源借样本）
    remainder: Vec<f32>,
    /// 声源种类。麦克风源启用 AGC（见 agc 字段），系统内放不启用。
    kind: SourceKind,
    /// **自动增益控制**（仅麦克风源 Some）。
    ///
    /// ## 为什么麦克风必须开 AGC（2026-10-24 用户：只说麦克风识别不到）
    /// 麦克风音量受设备/距离/系统增益影响，很多人默认输入电平极低
    /// （RMS 常低于 VAD 自适应噪声底），说话根本触发不了 VAD → 零字幕。
    /// AGC 带语音门控（快攻慢放）：检测到人声把电平抬到目标、安静时回落
    /// 不乱放大底噪。系统内放是数字信号、电平稳定，不需要也不应放大。
    agc: Option<Agc>,
    /// 说话人标签，写进字幕行
    speaker: String,
    /// 单调时钟基准（本源的第一个块）
    base_ms: Option<u64>,
    /// **长生命周期** ASR 会话（整个会话只有这一个）。
    ///
    /// ## 为什么不能每段新建（2026-10-23 实测结论）
    ///
    /// 用真实 zipformer 模型 + 真实英语语音（OSR 语料）做过对照实验，
    /// 同一素材下的产出文本量（以「一次喂完不切分」为 100% 上界）：
    ///
    /// | 方案 | 达上界 |
    /// |---|---|
    /// | 每段新建会话（旧实现） | 29% |
    /// | 每段新建 + 静音 padding | 58% |
    /// | 复用会话但丢弃段间静音 | 88% |
    /// | **复用会话 + 音频连续 + VAD 断句（本实现）** | **100%** |
    ///
    /// 流式 zipformer 依赖**跨段上下文累积**：新 session 没有左上下文，
    /// 段首内容大面积丢失（实测「MOOD PLA」/「UCK ROUND」被拦腰截断）。
    /// 所以会话必须复用。
    ///
    /// ## 为什么音频要连续喂（不能只喂 VAD 切出的语音段）
    ///
    /// 上表第 3 行（88%）与第 4 行（100%）的唯一差别就是**段间静音**：
    /// 静音是 sherpa 判断词边界的依据，丢掉它词就会粘连。
    /// 所以本实现把**所有**音频（含静音）连续喂进会话，
    /// VAD 只负责「何时断句」与提供时间戳。
    asr_session: Option<Box<dyn AsrSession>>,
    /// 已喂给 ASR 的累计样本数（用于把 sherpa 的相对时间戳换算成绝对时间）
    asr_fed_samples: usize,
    /// 最近一次**流式预览**文本。段结束时若 ASR 没吐字，用它兜底成行。
    ///
    /// ## 为什么需要（2026-10-24 用户实测「下面识别到了，但没进字幕和转写」）
    ///
    /// VAD 切段后 `finalize_segment()` 有时返回空（endpoint 未命中 / 尾部静音
    /// 不足），可此时预览里明明已经有字。旧实现直接丢弃 → 用户眼睁睁看着
    /// 字出现在预览区，却永远不进字幕列表与转写盘（表现为「丢字」）。
    last_partial: String,
}

/// 引擎线程的全部状态
struct EngineCtx {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    /// 识别后端。监督线程构建好后直接交给引擎循环。
    asr: Arc<dyn AsrBackend>,
    /// 暂停标志。引擎线程每轮检查它。
    paused: Arc<AtomicBool>,
    /// 多声源。第一个是主源（用于时间基准）。
    sources: Vec<AudioSource>,
    mt_tx: SyncSender<MtJob>,
    done_rx: Receiver<MtDone>,
    ev_tx: mpsc::Sender<LineEvent>,
    sink: Box<dyn TranscriptSink>,
    /// 内存窗口上限。超出的行被丢弃，转写盘仍保留。
    ///
    /// 注意档位不在这里 —— Segmenter 在 [`SessionEngine::start_multi`]
    /// 里就按 `cfg.profile` 建好了，每个源一个。
    memory_window_lines: usize,
    /// 两遍解码**第二遍**的离线精修器。`None` = 单遍流式。
    refiner: Option<crate::asr::offline::OfflineRefiner>,
    /// 运行时可调参数（与 `shared.live` 同一 Arc）。
    live: Arc<LiveConfig>,
    /// ASR 配置模板 —— 线程热切换时按它 clone 出新配置重建 recognizer。
    asr_config: crate::asr::AsrConfig,
    /// 当前 recognizer 实际生效的线程数（用于检测是否需要热切换）。
    applied_threads: usize,
    /// 热词快照（重建 session 时用；词库热更新仍需重建会话，见 start_multi 注释）。
    hotwords: Vec<String>,
}

/// 引擎线程主体
fn engine_loop(mut ctx: EngineCtx) -> SessionSummary {
    // 档位在 start_multi 里通过 Segmenter::new(cfg.profile) 生效。
    // 每个声源一个 Segmenter（见 AudioSource 的说明）。
    let mut assembler = SubtitleAssembler::new(0);
    // 必须在任何 commit 之前设置 —— 缩窗会立即丢弃队首
    assembler.set_max_lines(ctx.memory_window_lines);
    // 两遍解码第二遍：离线精修器提前取出，避免段循环内反复借 ctx
    let refiner = ctx.refiner.take();
    // ── 运行时可调参数（2026-10-25：UI 拖动 Slider 即时生效）──
    let live = Arc::clone(&ctx.live);
    let mut applied_threads = ctx.applied_threads;
    // 延迟初始化：循环第一轮就赋值，避免「初始值从未使用」警告
    let mut refine_enabled;
    let mut stats = PipelineStats::default();
    // 上一轮是否处于暂停（用于边沿检测）
    let mut was_paused = false;

    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }

        // ── 运行时可调参数（2026-10-25）──
        //
        // 1) hangover / 精修开关：**立即生效** —— 只改 Segmenter 字段与一个
        //    布尔，无需任何重建，零风险。
        // 2) ASR 线程数：sherpa 的线程数在 recognizer **创建时定死**，
        //    只能重建。切换点选在这里（句边界前）：当前段已处理完，
        //    不会打断正在识别的句子。重建后新 session 无左上下文，
        //    **切换后第一句的段首质量可能短暂下降** —— 这是本项目实测过
        //    的坑（每段新建会话文本产出只有 100% 的 29%），UI 必须提示用户。
        let want_hangover = live.hangover();
        for src in ctx.sources.iter_mut() {
            if src.segmenter.hangover_ms() != want_hangover {
                src.segmenter.set_hangover_ms(want_hangover);
            }
        }
        refine_enabled = live.refine_enabled();
        let want_threads = live.threads();
        if want_threads != applied_threads && !ctx.sources.is_empty() {
            eprintln!("[live] 热切换 ASR 线程 {applied_threads} → {want_threads}");
            let mut new_cfg = ctx.asr_config.clone();
            new_cfg.num_threads = want_threads;
            match super::build_asr_backend(&new_cfg, super::BackendChoice::Auto) {
                Ok(new_asr) => {
                    for src in ctx.sources.iter_mut() {
                        src.asr_session = new_asr.create_session(ctx.hotwords.clone()).ok();
                        src.last_partial.clear();
                    }
                    ctx.asr = new_asr;
                    applied_threads = want_threads;
                    eprintln!("[live] 热切换完成（新 recognizer 已就绪）");
                }
                Err(e) => {
                    eprintln!("[live] 热切换失败（继续用 {applied_threads} 线程）: {e}");
                    // 不重试：否则每轮都会重新加载模型、疯狂刷错误
                    applied_threads = want_threads;
                }
            }
        }

        // ── 暂停：不消费音频、不识别 ──
        //
        // 但**在途译文仍要 drain** —— 否则暂停前说的一句话，
        // 译文会一直卡在队列里，暂停结束才突然冒出来。
        //
        // 用边沿检测（was_paused）而不是 swap —— swap 会在每轮
        // 把暂停标志清掉，导致暂停只维持一轮就自己恢复。
        let now_paused = ctx.paused.load(Ordering::Relaxed);
        if !now_paused && was_paused {
            // 刚恢复：排空暂停期间积压的音频 —— 那是暂停之前几秒的
            // 旧内容，用户早就不想录了，识别出来只会莫名其妙。
            for src in ctx.sources.iter_mut() {
                while src.rx.try_recv().is_ok() {}
            }
        }
        was_paused = now_paused;
        if now_paused {
            drain_mt(&mut ctx, &mut assembler, &mut stats);
            std::thread::sleep(AUDIO_POLL);
            continue;
        }

        // ── 第一步：把音频喂给 ASR（连续）+ 用 VAD 断句 ──
        //
        // 不能边遍历 sources 边调 commit_line(&mut ctx) ——
        // 那是「同一个 ctx 被借两次」，Rust 不会放行。
        // 分成两步后借用互不重叠，逻辑也更好读。
        //
        // ## 关键：音频**连续**喂进 ASR，段间静音也要喂 ──
        //
        // 旧实现是「VAD 切段 → 每段新建会话 → 整段喂入 → finish」，
        // 实测只能认出上界 29% 的文本（流式模型缺左上下文）。
        // 现在：所有音频连续喂进**同一个**长生命周期会话，
        // VAD 只负责「何时断句」与提供时间戳。
        let mut pending: Vec<(SpeechSegment, String, Vec<AsrEvent>)> = Vec::new();
        // any_alive = 「还有源处于连接状态」。注意区分：
        //   Empty        → 连着但暂时没数据，**不能退出**
        //   Disconnected → 发送方没了，本源作废
        // 把 Empty 当成断开会让静音期直接终止会话。
        let mut any_alive = false;
        // 本轮是否真的抽到了音频。抽不到才 sleep。
        let mut drained_any = false;

        for (src_index, src) in ctx.sources.iter_mut().enumerate() {
            // 内层循环：把这个源的队列**抽干**再走。
            //
            // 为什么必须抽干：一个采集块只有 320 样本（20ms），
            // 而 VAD 一帧要 512 样本（32ms）—— 两块才凑得一帧。
            // 若每轮只取一块就 sleep(50ms)，3900 块要 195 秒才能喂完，
            // 一帧都凑不出来。抽干后一轮就能出帧。
            loop {
                match src.rx.try_recv() {
                    Ok(mut chunk) => {
                        any_alive = true;
                        drained_any = true;
                        let base = chunk.capture_time_ms;
                        // 麦克风源：先过 AGC 抬升电平，再喂 ASR / 切 VAD 帧。
                        // 顺序很关键 —— AGC 必须在两者之前，否则 VAD 用原始
                        // 低电平判静音、ASR 也听不清。
                        if let Some(agc) = src.agc.as_mut() {
                            agc.process(&mut chunk.samples);
                        }
                        if src.base_ms.is_none() {
                            src.base_ms = Some(base);
                        }
                        // ── 音频连续喂入长生命周期 ASR 会话 ──
                        //
                        // 必须在 VAD 切段**之前**喂：段间静音也是
                        // sherpa 判断词边界的依据，丢了词就会粘连。
                        //
                        // endpoint 在句中停顿断出的 Final 若不接住就会被
                        // drain 的 reset 永久丢弃（「停顿丢字」根因）——
                        // 这里收集，转成 pending 项按时间顺序 commit_line。
                        let endpoint_finals = feed_asr(
                            &ctx.shared,
                            ctx.asr.as_ref(),
                            src,
                            &chunk.samples,
                            &mut stats,
                            base,
                        );
                        for (text, bms) in endpoint_finals {
                            let seg = SpeechSegment {
                                samples: Vec::new(),
                                start_ms: bms,
                                end_ms: bms,
                                forced: false,
                            };
                            pending.push((
                                seg,
                                src.speaker.clone(),
                                vec![AsrEvent::Final {
                                    text,
                                    start_s: 0.0,
                                    end_s: 0.0,
                                    tokens: Vec::new(),
                                }],
                            ));
                        }

                        let frames = split_into_vad_frames(&chunk, &mut src.remainder, base);
                        for (frame, frame_time) in frames {
                            let Some(seg) = src.segmenter.push_frame(&frame, frame_time) else {
                                continue;
                            };
                            // ⚠️ **必须在这里立即 finalize**，不能攒到循环末尾。
                            //
                            // ASR 会话是长生命周期的，它累积了「自上次 finalize 以来」
                            // 的全部音频。若等抽干完再统一 finalize，
                            // 第一次 finalize 会把后面好几段的文本一起吞掉，
                            // 后续 finalize 反而什么都拿不到（已被 reset）。
                            //
                            // 在这里 finalize = 每段只交出自己那一段的文本，
                            // 与 sherpa 的 reset 语义一致。
                            let mut events = match src.asr_session.as_mut() {
                                Some(session) => match session.finalize_segment() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        stats.asr_errors += 1;
                                        Vec::new()
                                    }
                                },
                                None => Vec::new(),
                            };
                            // ── 两遍解码第二遍：离线模型重识别整段音频 ──
                            //
                            // 音频来自 VAD 段（`seg.samples`，已过 AGC 的 16k mono）。
                            //
                            // ⚠️ 三条安全边界（2026-10-25 用户实测
                            // 「讲得快时内容被丢弃、字幕显示一会就消失」的修复）：
                            //
                            // 1. **只在段内恰好 1 个 Final 时修正**。
                            //    快语速时 sherpa endpoint 会在句中多次命中，
                            //    `finalize_segment` 返回**多个** Final。旧实现把
                            //    第 2..N 个 `clear()`，而 Final 会被逐个
                            //    `commit_line` 成行 → 那些句子以**空文本**成行，
                            //    内容彻底丢失（既不上字幕也不进转写盘）。
                            //    离线模型对整段只给一个文本，无法映射回多句，
                            //    所以多 Final 时**整体跳过**，完整保留流式结果。
                            // 2. **长度保护**：离线文本不足流式 60% 时，
                            //    认为离线丢了更多内容 → 保留流式。
                            // 3. 离线失败 / 空结果 → 保留流式
                            //    （字幕绝不能因第二遍失败而消失）。
                            // 4. `refine_enabled` 为 false（用户在设置里实时关掉
                            //    精修）时同样跳过 —— 立即生效，无需重启。
                            if refine_enabled {
                                if let Some(refiner) = &refiner {
                                let final_count = events
                                    .iter()
                                    .filter(|e| matches!(e, AsrEvent::Final { .. }))
                                    .count();
                                if final_count > 1 {
                                    eprintln!(
                                        "[refine] 段内 {} 个 Final（快语速），跳过精修、保留流式",
                                        final_count
                                    );
                                } else if final_count == 1 && !seg.samples.is_empty() {
                                    let stream_text = events
                                        .iter()
                                        .find_map(|e| match e {
                                            AsrEvent::Final { text, .. } => Some(text.clone()),
                                            _ => None,
                                        })
                                        .unwrap_or_default();
                                    let stream_chars = stream_text.chars().count();
                                    match refiner.refine(&seg.samples) {
                                        // 长度达标才接受修正
                                        Some(text)
                                            if text.chars().count() * 10 >= stream_chars * 6 =>
                                        {
                                            eprintln!(
                                                "[refine] 流式 {} 字 → 离线 {} 字",
                                                stream_chars,
                                                text.chars().count()
                                            );
                                            for e in events.iter_mut() {
                                                if let AsrEvent::Final { text: t, .. } = e {
                                                    *t = text.clone();
                                                }
                                            }
                                        }
                                        Some(text) => {
                                            eprintln!(
                                                "[refine] 长度异常（离线 {} 字 < 流式 {} 字 × 60%），保留流式",
                                                text.chars().count(),
                                                stream_chars
                                            );
                                        }
                                        None => {}
                                    }
                                }
                            }
                            } // if refine_enabled
                            // 兜底：finalize 没吐出非空文本，但预览里有字 ——
                            // 用预览成行，绝不把已经识别出来的字丢掉。
                            let has_text = events.iter().any(|e| match e {
                                AsrEvent::Final { text, .. } => !text.trim().is_empty(),
                                _ => false,
                            });
                            if !has_text && !src.last_partial.trim().is_empty() {
                                events.push(AsrEvent::Final {
                                    text: std::mem::take(&mut src.last_partial),
                                    start_s: 0.0,
                                    end_s: 0.0,
                                    tokens: Vec::new(),
                                });
                            } else {
                                src.last_partial.clear();
                            }
                            pending.push((seg, src.speaker.clone(), events));
                        }
                    }
                    // Empty = 还连着但暂时没数据。**必须置 any_alive** ——
                    // 否则启动后第一轮（音频还没送来）就会误判为
                    // 「所有源都断开」而立刻退出，一个字幕都不产出。
                    Err(TryRecvError::Empty) => {
                        any_alive = true;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        // ── 第二步：借 ctx 处理段 ──
        for (seg, speaker, events) in pending {
            stats.vad_segments += 1;
            for ev in events {
                match ev {
                    AsrEvent::Partial { text, .. } => publish(&ctx, &stats, 0, Some(text)),
                    AsrEvent::Final { text, .. } => {
                        commit_line(
                            &mut ctx,
                            &mut assembler,
                            &text,
                            &seg,
                            &speaker,
                            &mut stats,
                        )
                    }
                    // 断句权独占给 VAD，引擎内的 boundary 一律忽略。
                    AsrEvent::SegmentBoundary => {}
                }
            }
        }

        drain_mt(&mut ctx, &mut assembler, &mut stats);

        // 所有源都断开才退出。静音期（Empty）不算断开 ——
        // 否则用户停顿时会话就自己结束了。
        if !any_alive {
            break;
        }
        // 本轮没抽到音频才让出 CPU。抽到了就立刻继续下一轮，
        // 多源并发时一帧都不能等。
        if !drained_any {
            std::thread::sleep(AUDIO_POLL);
        }
    }

    // 冲刷：每个源都要把最后一段没说完的变成字幕行
    {
        let mut pending: Vec<(SpeechSegment, String, Vec<AsrEvent>)> = Vec::new();
        for src in ctx.sources.iter_mut() {
            while let Some(seg) = src.segmenter.flush() {
                // 收尾段同样立即 finalize
                let events = match src.asr_session.as_mut() {
                    Some(session) => match session.finalize_segment() {
                        Ok(v) => v,
                        Err(_) => {
                            stats.asr_errors += 1;
                            Vec::new()
                        }
                    },
                    None => Vec::new(),
                };
                pending.push((seg, src.speaker.clone(), events));
            }
        }
        for (seg, speaker, events) in pending {
            stats.vad_segments += 1;
            for ev in events {
                if let AsrEvent::Final { text, .. } = ev {
                    commit_line(
                        &mut ctx,
                        &mut assembler,
                        &text,
                        &seg,
                        &speaker,
                        &mut stats,
                    );
                }
            }
        }
        // 会话结束：把 ASR 里还没交出来的最后一句冲出来。
        //
        // 先把事件收集到局部变量，再统一 commit_line ——
        // 直接在 `ctx.sources.iter_mut()` 里调 commit_line(&mut ctx) 会双重借用。
        let mut trailing: Vec<(String, u64, String)> = Vec::new();
        for src in ctx.sources.iter_mut() {
            if let Some(session) = src.asr_session.as_mut() {
                if let Ok(events) = session.finish() {
                    for ev in events {
                        if let AsrEvent::Final { text, .. } = ev {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            trailing.push((text, src.base_ms.unwrap_or(0), src.speaker.clone()));
                        }
                    }
                }
            }
        }
        for (text, base, speaker) in trailing {
            stats.asr_finals += 1;
            let seg = SpeechSegment {
                samples: Vec::new(),
                start_ms: base,
                end_ms: base,
                forced: true,
            };
            commit_line(
                &mut ctx,
                &mut assembler,
                &text,
                &seg,
                &speaker,
                &mut stats,
            );
        }
    }
    assembler.finish();

    // 等在途译文。停止必须把最后一句的译文也带出来，
    // 否则用户看到「说了但没翻」的半句话。
    drain_mt_budget(&mut ctx, &mut assembler, &mut stats, MT_FLUSH_BUDGET);

    ctx.sink.finish();
    publish(&ctx, &stats, assembler.peek_next_seq().saturating_sub(1), None);

    let transcript_path = ctx.sink.path();
    let transcript_written = ctx.sink.written();
    let transcript_failures = ctx.sink.failures();
    SessionSummary {
        srt: assembler.export_srt(),
        vtt: assembler.export_vtt(),
        txt: assembler.export_txt(),
        lines: assembler.lines().to_vec(),
        stats,
        transcript_path,
        transcript_written,
        transcript_failures,
    }
}

/// 把音频连续喂进该声源的**长生命周期** ASR 会话，产出流式预览（partial），
/// 并返回本轮 endpoint 断出的 Final（文本 + 该 chunk 起始时刻 ms）。
///
/// 会话在第一次喂音频时创建（懒创建），之后整个会话复用同一个实例 ——
/// 这是 2026-10-23 的核心修复（见 [`AudioSource::asr_session`] 实测数据）。
///
/// ## 为什么必须取 partial（2026-10-24「没有流式输出」）
/// `AsrSession::accept` 每轮返回 Partial（解码器当前结果）。旧实现只判
/// `is_err()`、把事件整批丢弃 → 引擎从不发布预览，观感就是「没有流式」。
/// 现在每轮 Partial 立即写进共享快照，UI 轮询即可看到边说边出字。
///
/// ## 为什么必须把 endpoint Final 交出去（2026-10-24「停顿丢字」根因）
///
/// sherpa 的 endpoint 检测会在**句中停顿**时提前判定"这句说完了"，
/// drain 内部随即产出 Final 并 `reset` 解码器。旧实现 `feed_asr` 里
/// `if let AsrEvent::Partial` **只处理预览、把 Final 整批丢弃** —— 而
/// reset 已经发生，那句话的音频状态被清空，文字就永久丢了。表现即用户
/// 说的「稍微停一会再继续讲，会丢掉部分内容」。
/// 现在把这些 Final 收集返回，由主循环按时间顺序 commit_line 成行。
fn feed_asr(
    shared: &std::sync::Arc<Shared>,
    backend: &dyn AsrBackend,
    src: &mut AudioSource,
    samples: &[f32],
    stats: &mut PipelineStats,
    base_ms: u64,
) -> Vec<(String, u64)> {
    let mut endpoint_finals = Vec::new();
    if samples.is_empty() {
        return endpoint_finals;
    }

    // 懒创建会话。会话在会话开始时已按词库快照建好（见 start_multi），
    // 这里只是兜底（理论上不会走到）。
    //
    // 热词：会话创建时固定。词库热更新需要新会话才生效，这是有意的取舍 ——
    // 重建会话会丢上下文（实测从 100% 掉到 29%），代价远大于热词延迟生效。
    if src.asr_session.is_none() {
        match backend.create_session(Vec::new()) {
            Ok(s) => src.asr_session = Some(s),
            Err(_) => {
                stats.asr_errors += 1;
                return endpoint_finals;
            }
        }
    }
    let session = match src.asr_session.as_mut() {
        Some(s) => s,
        None => return endpoint_finals,
    };
    src.asr_fed_samples += samples.len();
    match session.accept(samples) {
        Ok(events) => {
            // Partial → 流式预览；endpoint 的 Final → 收集返回，主循环成行。
            for ev in events {
                match ev {
                    AsrEvent::Partial { text, .. } => {
                        let t = text.trim();
                        if !t.is_empty() {
                            // 记下来：段结束若 ASR 没吐字，用它兜底成行（防丢字）
                            src.last_partial = t.to_string();
                            publish_partial(shared, stats, t);
                        }
                    }
                    // endpoint 断出的 Final：drain 内部已 reset，这里若不
                    // 接住就会永久丢失（「停顿丢字」根因）。收集返回，
                    // 主循环按时间顺序 commit_line。
                    AsrEvent::Final { text, .. } => {
                        let t = text.trim();
                        if !t.is_empty() {
                            endpoint_finals.push((t.to_string(), base_ms));
                            // endpoint 已把这句交出去，预览槽位清空，
                            // 避免随后 VAD finalize 又用 last_partial 兜底重复成行
                            src.last_partial.clear();
                        }
                    }
                    AsrEvent::SegmentBoundary => {}
                }
            }
        }
        Err(_) => stats.asr_errors += 1,
    }
    endpoint_finals
}

/// 提交一行：EN 立即上屏（MT 背压），翻译异步派发
fn commit_line(
    ctx: &mut EngineCtx,
    assembler: &mut SubtitleAssembler,
    text: &str,
    seg: &SpeechSegment,
    speaker: &str,
    stats: &mut PipelineStats,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    // 英文大小写恢复：双语 zipformer 对英文输出几乎全大写
    // （"THREE" / "JACK WATCH"），用户反馈「大小写不对」。
    // 在**送 MT 之前**恢复 —— 译文与字幕两处同时受益。
    // 非全大写英文（正常文本/纯中文）原样返回，零副作用。
    let text = crate::text_normalize::recapitalize_english(text);
    let text = text.as_str();
    stats.asr_finals += 1;

    // Layer B 哨兵保护：术语在翻译期间被替换成 ▁X{n}X
    let guard = ctx.shared.glossary.guard();
    let pending = guard.start(text);

    let seq = assembler.commit(
        text.to_string(),
        None, // 译文未就绪 → 先入行后补（§6.2）
        seg.start_ms,
        seg.end_ms,
        seg.forced,
        false,
        Some(speaker.to_string()),
    );

    let rel_start = assembler.rel(seg.start_ms);
    let rel_end = assembler.rel(seg.end_ms);

    // 转写盘：EN 先落盘，比 UI 上屏还早
    ctx.sink
        .append_add(seq, text, rel_start, rel_end, seg.forced, speaker);

    // 立即上屏 —— 这一步不等待 MT
    let _ = ctx.ev_tx.send(LineEvent::Added {
        seq,
        source: text.to_string(),
        start_ms: rel_start,
        end_ms: rel_end,
        forced_cut: seg.forced,
        speaker: Some(speaker.to_string()),
    });

    // 派翻译。队列满则**丢译文**而不是阻塞引擎线程 ——
    // 用户宁可看到没翻译的英文，也不能看到卡住的程序。
    match ctx.mt_tx.try_send(MtJob {
        seq,
        pending,
        enqueued_at: Instant::now(),
    }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => stats.mt_dropped += 1,
        Err(TrySendError::Disconnected(_)) => set_error(ctx, "翻译线程已退出".into()),
    }

    // ⚠️ 成行后**必须清空预览**（2026-10-24 用户反馈「流式行在重复上句」的
    // 根因）：不清的话，字幕窗的「当前行」会一直挂着上一句的文本，
    // 与刚定型的锁定行重复。清掉后当前行回到占位符，等下一句重新生长。
    publish(ctx, stats, seq, None);
}

/// 非阻塞处理已完成的译文
fn drain_mt(ctx: &mut EngineCtx, assembler: &mut SubtitleAssembler, stats: &mut PipelineStats) {
    while let Ok(done) = ctx.done_rx.try_recv() {
        apply_mt(ctx, assembler, stats, done);
    }
}

/// 停止时等待在途译文，最多 `budget`
fn drain_mt_budget(
    ctx: &mut EngineCtx,
    assembler: &mut SubtitleAssembler,
    stats: &mut PipelineStats,
    budget: Duration,
) {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match ctx.done_rx.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(done) => apply_mt(ctx, assembler, stats, done),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 引擎线程仍在跑（stop 未置位）就继续等；
                // 停止流程中则已无新任务，直接收手
                if ctx.stop.load(Ordering::Relaxed) {
                    // 再宽限一次，确认真的没有在途结果了
                    match ctx.done_rx.recv_timeout(Duration::from_millis(150)) {
                        Ok(done) => apply_mt(ctx, assembler, stats, done),
                        Err(_) => break,
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    ctx.sink.flush();
}

fn apply_mt(
    ctx: &mut EngineCtx,
    assembler: &mut SubtitleAssembler,
    stats: &mut PipelineStats,
    done: MtDone,
) {
    // 先落盘（无论行是否还在内存窗口里 —— 窗口外的那行也必须在盘上补到译文）
    ctx.sink
        .append_fill(done.seq, &done.target, done.needs_review);

    let in_window = assembler.fill_translation(done.seq, done.target.clone(), done.needs_review);
    if in_window {
        stats.mt_done += 1;
        let _ = ctx.ev_tx.send(LineEvent::Filled {
            seq: done.seq,
            target: done.target,
            needs_review: done.needs_review,
        });
    }
    publish(ctx, stats, done.seq, None);
}

/// MT 线程主体
/// 文本是否以中日韩字符为主（>40% 的非空白字符是 CJK）。
///
/// ## 为什么需要
///
/// 双语 ASR（zipformer-bilingual-zh-en）会直接输出中文。
/// 而 MT 模型是**英→中**（bergamot enzh）—— 把中文喂进英译中模型
/// 产出的是乱码。中文源文必须跳过翻译，原样上屏。
fn is_mostly_cjk(text: &str) -> bool {
    let mut cjk = 0usize;
    let mut total = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        let cp = c as u32;
        // CJK 统一表意文字 + 扩展A + 兼容表意 + 假名（顺带覆盖日源）
        if (0x4E00..=0x9FFF).contains(&cp)
            || (0x3400..=0x4DBF).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0x3040..=0x30FF).contains(&cp)
        {
            cjk += 1;
        }
    }
    // ⚠️ 2026-10-25 阈值 40%→80%（`cjk*10 >= total*4` → `*8`）。
    // 原阈值太松：中英混排句（含中文术语/人名的正常英文句）CJK 占比
    // 轻易超过 40% → 被误判成「中文源文」→ **整个跳过翻译**，
    // 译文留空。用户反馈「常见正常句子翻译不出来」的典型原因之一。
    // 80% = 只在真的是中文为主的句子上才跳过。
    total > 0 && cjk * 10 >= total * 8
}

/// 日志用：取文本前 60 字符（避免长句刷屏）
fn preview(s: &str) -> String {
    let count = s.chars().count();
    let t: String = s.chars().take(60).collect();
    if count > 60 {
        format!("{t}…")
    } else {
        t
    }
}

/// 流式译文线程主体（2026-10-24 用户需求：翻译也流式）。
///
/// 轮询共享快照的 `partial`（ASR 流式预览），每前进一点就**重译当前最新
/// 文本**写入 `partial_zh` —— bergamot 没有增量 KV cache，做的是全量重译，
/// 但对用户就是"译文跟着字长出来"。
///
/// 节流与时效：
/// - 只在 partial **变化**时翻译（`last == now` 直接跳过，零成本空转）。
/// - 翻译耗时期间新到的 partial 会让本轮结果作废、下一轮立即翻最新文本 ——
///   **天然只保留最新**，不会排队堆积（这正是流式预览要的行为：
///   宁可跳过中间态，也不让译文滞后于原文）。
/// - partial 被清空（成行）时同步清空 `partial_zh`。
/// - 纯中文预览跳过（英译中模型翻中文出乱码）。
fn partial_mt_loop(shared: Arc<Shared>, mt: SharedMt, stop: Arc<AtomicBool>) {
    // MT 不可用（阶段1 仅识别模式）：不翻译，也不写 partial_zh
    // （否则 UI 会一直显示一个空译文占位）。
    if !mt.is_available() {
        return;
    }
    let mut last_en = String::new();
    while !stop.load(Ordering::Relaxed) {
        // 快速取当前 partial（不长期持锁）
        let cur = match shared.published.lock() {
            Ok(p) => p.partial.clone().unwrap_or_default(),
            Err(_) => break, // 锁污染 → 线程退出（主链路不受影响）
        };
        if cur.trim().is_empty() {
            if !last_en.is_empty() {
                last_en.clear();
                if let Ok(mut p) = shared.published.lock() {
                    p.partial_zh = None;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(40)); // 流式译文：更快追上最新预览
            continue;
        }
        if cur == last_en {
            std::thread::sleep(std::time::Duration::from_millis(40)); // 流式译文：更快追上最新预览
            continue;
        }
        if is_mostly_cjk(&cur) {
            // 中文预览无需翻译
            last_en = cur;
            continue;
        }
        // 锁外翻译（可能数百 ms）
        let translated = mt.translate(&cur).map(|t| t.text).unwrap_or_default();
        // 写回前再确认：partial 若已变化/清空，结果过期 —— 但仍写入（下一轮
        // 会立即用最新文本覆盖，滞后仅一帧）；成行清空时不复活。
        if let Ok(mut p) = shared.published.lock() {
            if p.partial.as_deref() == Some(cur.as_str()) {
                last_en = cur;
            }
            if p.partial.is_some() {
                p.partial_zh = Some(translated);
            }
        }
    }
}

/// 译文回填后，若预览槽位已被主循环重新填充（用户已经开始说下一句），
/// 流式线程会自行追上 —— 这里无需处理。
fn mt_loop(jobs: mpsc::Receiver<MtJob>, mt: SharedMt, done: mpsc::Sender<MtDone>) {
    // MT 不可用（Android 阶段1 只装 ASR 模型）：译文恒空，
    // 但**行必须照常成行**（UI 的「翻译中」不能永远挂着）。
    // 逐行给出空译文 + 不标记待复核（不是翻译质量问题）。
    let mt_available = mt.is_available();
    if !mt_available {
        eprintln!(
            "[mt] 翻译后端不可用（{}）—— 进入「仅识别」模式，译文恒空",
            mt.unavailable_reason().unwrap_or_else(|| "原因未知".into())
        );
    }
    while let Ok(job) = jobs.recv() {
        let MtJob {
            seq,
            pending,
            enqueued_at,
        } = job;
        if !mt_available {
            let (zh, _) = pending.finish("");
            let _ = done.send(MtDone {
                seq,
                target: zh,
                needs_review: false,
            });
            continue;
        }
        // ── 中文源文跳过翻译 ──
        // 双语 ASR 直接吐中文时，英→中 MT 会产出乱码。
        // 原样 finish（哨兵仍要还原，术语替换照常生效）。
        if is_mostly_cjk(pending.text_for_translation()) {
            eprintln!(
                "[mt] seq={seq} 中文源文（跳过翻译）: {}",
                preview(pending.text_for_translation())
            );
            // 译文留空（不是重复源文）—— UI 第二行不显示，
            // 避免中文在「原文/译文」两行里重复出现。
            let (zh, _) = pending.finish("");
            let _ = done.send(MtDone {
                seq,
                target: zh,
                needs_review: false,
            });
            continue;
        }
        // translate 借用文本；finish 消费 PendingSegment。
        // 失败时也必须 finish 以消费掉它，否则哨兵映射被丢弃。
        let (target, needs_review) = match mt.translate(pending.text_for_translation()) {
            // 模型返回空译文：不是失败，但同样不出译文，必须记下来
            Ok(t) if t.text.trim().is_empty() => {
                eprintln!(
                    "[mt] seq={seq} 译文为空 | 原文: {}",
                    preview(pending.text_for_translation())
                );
                let (zh, _) = pending.finish("");
                (zh, true)
            }
            Ok(t) => {
                let (zh, restore) = pending.finish(&t.text);
                (zh, !restore.is_consistent())
            }
            Err(e) => {
                // 翻译失败：目标留空 + 标记待复核，
                // 否则 UI 会永远停在「翻译中」
                //
                // ⚠️ 2026-10-25：这里原来**完全静默**（不打印、不计数），
                // 用户反馈「常见句子翻译不出来」却查不到任何原因，就是它。
                eprintln!(
                    "[mt] seq={seq} 翻译失败: {e} | 原文: {}",
                    preview(pending.text_for_translation())
                );
                let (zh, _) = pending.finish("");
                (zh, true)
            }
        };
        // ── 时延诊断（2026-10-24）──
        // 「入队→出译文」= 排队 + bergamot 推理 的总时长。
        // 正常应 <1s；若普遍 3~10s 说明推理慢或串行排队堆积。
        let ms = enqueued_at.elapsed().as_millis();
        if ms >= 1000 {
            eprintln!(
                "[mt] seq={seq} 入队→出译文 {ms} ms（含排队；慢则调优 MT 线程/模型）"
            );
        }
        if done
            .send(MtDone {
                seq,
                target,
                needs_review,
            })
            .is_err()
        {
            break; // 引擎线程已退出
        }
    }
}

/// 空摘要（引擎没跑起来时用）
fn empty_summary() -> SessionSummary {
    SessionSummary {
        lines: Vec::new(),
        srt: String::new(),
        vtt: String::new(),
        txt: String::new(),
        stats: PipelineStats::default(),
        transcript_path: None,
        transcript_written: 0,
        transcript_failures: 0,
    }
}

/// 取出预置后端，或按配置构建
fn resolve_backends(
    shared: &Arc<Shared>,
    cfg: &PipelineConfig,
) -> Result<Backends> {
    if let Some(b) = &shared.backends {
        return Ok(Backends {
            asr: Arc::clone(&b.asr),
            mt: Arc::clone(&b.mt),
        });
    }
    let asr = super::build_asr_backend(&cfg.asr_config(), cfg.backend)?;
    let mt = super::build_mt_backend(&cfg.mt_config(), cfg.backend)?;
    Ok(Backends { asr, mt })
}

fn set_shared_error(shared: &Arc<Shared>, msg: String) {
    if let Ok(mut p) = shared.published.lock() {
        p.error = Some(msg);
    }
}

fn set_shared_state(shared: &Arc<Shared>, s: SessionState) {
    if let Ok(mut p) = shared.published.lock() {
        p.state = s;
    }
}

fn set_error(ctx: &EngineCtx, msg: String) {
    if let Ok(mut p) = ctx.shared.published.lock() {
        p.error = Some(msg);
    }
}

/// 只更新流式预览文本（供 `feed_asr` 每轮调用）。
///
/// 与 [`publish`] 的区别：不动 `last_seq`、不清空 partial —— 预览是
/// 「当前正在说的那句话」，由下一次 Partial 或切段成行来推进。
fn publish_partial(shared: &std::sync::Arc<Shared>, stats: &PipelineStats, text: &str) {
    if let Ok(mut p) = shared.published.lock() {
        p.stats = stats.clone();
        p.partial = Some(text.to_string());
    }
}

fn publish(ctx: &EngineCtx, stats: &PipelineStats, last_seq: u64, partial: Option<String>) {
    if let Ok(mut p) = ctx.shared.published.lock() {
        p.stats = stats.clone();
        // ⚠️ 必须先判断「序号是否前进」再更新 last_seq（2026-10-24 定位）：
        // 旧代码先把 p.last_seq 提到新值，再判 last_seq > p.last_seq —— 条件
        // 永远为假，**成行后预览永远清不掉**。后果：字幕窗的当前行一直挂着
        // 上一句、与刚定型的锁定行重复，看着像整句直接蹦出来、没有流式生长。
        let seq_advanced = last_seq > p.last_seq;
        p.last_seq = p.last_seq.max(last_seq);
        if let Some(t) = partial {
            p.partial = Some(t);
        } else if seq_advanced {
            p.partial = None;
            p.partial_zh = None; // 成行 → 流式译文槽位让位给正式行
        }
    }
}