//! 会话级 FFI：建会话、启停、取事件、查询转写盘。
//!
//! 与 [`crate::ffi`] 的静态函数不同，这里是**有状态**的：
//! 一个会话持有一路采集 + 一个引擎 + 一个转写盘写入器。

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use crate::audio::PcmChunk;
use crate::config::AppConfig;
use crate::error::{Error, Result};
use crate::ffi::{CaptureHandle, FfiError};
use crate::pipeline::engine::{SessionEngine, SessionState};
use crate::pipeline::{LineEvent, SourceKind, SourceSpec};
use crate::transcript::writer::TranscriptWriter;

/// 会话内部状态。
struct FfiSession {
    /// 会话配置
    #[allow(dead_code)]
    config: AppConfig,
    /// 引擎（Mutex：FFI 只能拿到 `*const c_void`）
    engine: Mutex<SessionEngine>,
    /// 采集器列表
    capture: Mutex<Vec<CaptureHandle>>,
    /// 会话汇总（行、写入计数等）
    summary: Mutex<Option<crate::pipeline::engine::SessionSummary>>,
    /// 转写盘写入器
    writer: Mutex<Option<TranscriptWriter>>,
    /// 事件队列（引擎 → UI）
    events: Mutex<Vec<LineEvent>>,
    /// 序号
    seq: Mutex<u64>,
    /// 转写盘目录（可选）。`None` = 不写转写盘。
    transcript_path: Mutex<Option<PathBuf>>,
    /// 运行时：0=原生，1=卓易通容器。
    runtime: i32,
    /// 停止 worker 的完成信号（Some = 停止流程已启动）。
    ///
    /// ## 为什么停止必须异步（2026-10-23 用户实测「未响应」根因）
    ///
    /// 旧实现 `stop` 在 FFI 线程（= Dart UI isolate）上同步 join 引擎/
    /// MT 线程。任一线程不退出（MT 卡在 bergamot translate、引擎卡在
    /// sherpa 推理），join 永久阻塞 → **整个 UI 未响应**。
    ///
    /// 现在：stop 起一个 worker 线程做真正的 join，FFI 侧只等**短超时**
    /// （stop 3s / free 15s）。超时后按社区标准做法放弃 join（leak 线程，
    /// 它最终会自己退出），UI 永不卡死。worker 完成时经 channel 报信，
    /// free 收到信才安全 drop 会话。
    stop_done_rx: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// **期望的声源**（人类可读标签）。
    ///
    /// ## 为什么需要它
    ///
    /// 采集器创建失败（无音频设备 / 权限未授予）时，
    /// `capture` 列表是空的 —— 但 UI 仍需要知道「用户选了哪个声源、
    /// 为什么不可用」。这个列表与采集器是否存在**解耦**：
    /// 采集器建好了就用真标签，建不好就用这里记的。
    expected_sources: Mutex<Vec<String>>,
}

/// 会话状态（供 UI 查询）。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FfiSessionStatus {
    /// 0=已创建 1=运行中 2=已停止 3=出错
    pub state: i32,
    /// ASR 引擎是否就绪
    pub asr_ready: i32,
    /// MT 引擎是否就绪
    pub mt_ready: i32,
    /// 错误码（0 = 无）
    pub error_code: i32,
    /// 已产出的最后一行 seq
    pub last_seq: u64,
    /// 内存窗口内行数
    pub line_count: u64,
    /// 采集丢帧数
    pub dropped_frames: u64,
    /// 因 MT 队列满而丢弃的译文数
    pub mt_dropped: u64,
    /// 转写盘已写入记录数
    pub transcript_written: u64,
    /// 转写盘写失败次数
    pub transcript_failures: u64,
}

/// 转写盘统计（供 UI 查询）。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FfiTranscriptStats {
    /// 已写入记录数
    pub written: u64,
    /// 写失败次数
    pub failures: u64,
    /// 最后一行 seq
    pub last_seq: u64,
}

/// 把 Rust 字符串转成 C 字符串（堆分配，调用方负责 free）。
fn to_c_string(s: &str) -> *mut c_char {
    CString::new(s)
        .map(|c| c.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// JSON 字符串转义。
///
/// **自带首尾引号** —— 与 [`crate::transcript::record::json_escape`] 同一套规则，
/// 免得 FFI 与转写盘各写一套。调用处直接拼进 JSON 即可，
/// 不要再手动加引号（那会双重转义 → 非法 JSON）。
fn esc(s: &str) -> String {
    crate::transcript::record::json_escape(s)
}


/// 从 JSON 构造会话配置。
///
/// ## 设计
///
/// - **未知键直接忽略** —— 新版本加字段不应让旧版 UI 崩。
/// - **空 `transcript_dir` 视为未设置** —— Dart 侧把「用户没选目录」
///   传成空串，不能被当成磁盘根目录。
/// - **坏 JSON 返回 Err** —— 让 UI 能如实报「配置解析失败」。
fn config_from_json(json: &str) -> Result<crate::pipeline::PipelineConfig> {
    use crate::config::PerformanceProfile;
    use crate::pipeline::wiring::BackendChoice;

    let v = crate::glossary::parse::json::parse_value(json)
        .map_err(|e| Error::InvalidConfig(format!("配置 JSON 解析失败: {e}")))?;

    let mut cfg = crate::pipeline::PipelineConfig::default();

    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64());

    if let Some(x) = s("asr_model_dir") {
        cfg.asr_model_dir = PathBuf::from(x);
    }
    if let Some(x) = s("mt_model_dir") {
        cfg.mt_model_dir = PathBuf::from(x);
    }
    // 两遍解码第二遍的离线模型目录（空串 = 不启用）
    if let Some(x) = s("offline_model_dir") {
        cfg.offline_model_dir = PathBuf::from(x);
    }
    if let Some(x) = b("offline_refine") {
        cfg.offline_refine = x;
    }
    if let Some(arr) = v.get("glossary_paths").and_then(|x| x.as_array()) {
        cfg.glossary_paths = arr
            .iter()
            .filter_map(|x| x.as_str().map(PathBuf::from))
            .collect();
    }
    if let Some(x) = s("profile") {
        cfg.profile = match x.as_str() {
            "fast" => PerformanceProfile::Fast,
            _ => PerformanceProfile::Stable,
        };
    }
    // ⚠️ 2026-10-25 修复：resource_tier 此前**从未被解析** —— Dart 一直
    // 在传，Rust 直接忽略，导致用户在设置页切换轻量/均衡/全力完全无效
    // （用户实测"提到 4 效果不大"就是因为这个：实际始终跑默认档）。
    // 必须放在 asr_threads / mt_threads **之前**解析：
    // 显式线程数优先级更高，后解析会覆盖档位的联动值。
    if let Some(x) = s("resource_tier") {
        cfg.set_resource_tier(crate::config::ResourceTier::from_str(&x));
    }
    if let Some(x) = n("asr_threads") {
        cfg.asr_threads = x.max(1) as usize;
    }
    if let Some(x) = n("mt_threads") {
        cfg.mt_threads = x.max(1) as usize;
    }
    if let Some(x) = b("mt_accurate") {
        cfg.mt_accurate = x;
    }
    // MT 是否可选（Android 阶段1：只装 ASR 模型，MT 权重未就位）
    if let Some(x) = b("mt_optional") {
        cfg.mt_optional = x;
    }
    if let Some(x) = n("memory_window_lines") {
        cfg.memory_window_lines = x.max(1) as usize;
    }
    if let Some(x) = s("backend") {
        // BackendChoice 只有 Auto / Mock 两个变体：
        // Auto = 按编译 feature 选真实引擎（sherpa / bergamot），
        // Mock = 强制用内置参考实现。
        cfg.backend = match x.as_str() {
            "mock" => BackendChoice::Mock,
            _ => BackendChoice::Auto,
        };
    }
    // 空串 = 未选择目录 → 不写转写盘
    if let Some(x) = s("transcript_dir") {
        if !x.is_empty() {
            cfg.transcript_dir = Some(PathBuf::from(x));
        }
    }
    // 声源
    if let Some(arr) = v.get("sources").and_then(|x| x.as_array()) {
        cfg.sources = arr
            .iter()
            .filter_map(|x| {
                let kind = match x.get("kind").and_then(|k| k.as_str())? {
                    "microphone" => SourceKind::Microphone,
                    _ => SourceKind::SystemAudio,
                };
                let speaker = x
                    .get("speaker")
                    .and_then(|k| k.as_str())
                    .map(|k| k.to_string());
                Some(match speaker {
                    Some(sp) => SourceSpec::with_speaker(kind, sp),
                    None => SourceSpec::new(kind),
                })
            })
            .collect();
    }
    Ok(cfg)
}

/// 创建会话。
///
/// `cfg_json` 为配置 JSON（见 [`config_from_json`]）。
/// `source` = 0 内放 / 1 麦克风，`runtime` = 0 原生 / 1 卓易通容器。
///
/// 返回 null 表示配置非法或引擎不可用 —— UI 应据此提示，
/// 而不是崩溃。
///
/// # Safety
/// `cfg_json` 必须是有效的 C 字符串指针（可为 null）。
#[no_mangle]
pub unsafe extern "C" fn translator_session_create(
    cfg_json: *const c_char,
    source: i32,
    runtime: i32,
) -> *mut c_void {
    if cfg_json.is_null() {
        return std::ptr::null_mut();
    }
    // source 越界 → 拒绝
    let kind = match source {
        0 => SourceKind::SystemAudio,
        1 => SourceKind::Microphone,
        _ => return std::ptr::null_mut(),
    };
    // runtime 越界 → 拒绝
    if !(0..=1).contains(&runtime) {
        return std::ptr::null_mut();
    }

    let cfg_str = match CStr::from_ptr(cfg_json).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let cfg = match config_from_json(cfg_str) {
        Ok(c) => c,
        Err(_) => return std::ptr::null_mut(),
    };

    // ⚠️ 2026-10-27：真机可观测性（Android 上 eprintln! 不进 logcat）。
    crate::alog!(
        "session_create 开始 backend={:?} asr={} mt_optional={} sources={}",
        cfg.backend,
        cfg.asr_model_dir.display(),
        cfg.mt_optional,
        cfg.sources.len()
    );

    // 声源规格：内放 / 麦克风。
    //
    // ⚠️ 只有当 JSON **没配** sources 时，才用 `source` 参数兜底建单源
    // （2026-10-24 用户实测「对麦克风说话不出字」根因）：旧代码无条件
    // `cfg.sources = vec![SourceSpec::new(kind)]`，把 UI 传来的「内放+麦克风」
    // 双源 JSON 强行压成单源 —— 而 UI 的 source 参数默认是内放(0)，
    // 于是麦克风采集器根本没建立，对麦克风说话永远识别不到。
    // 现在尊重 JSON 多源：双源同时采集，各自 VAD 门控，静音那路不产字，
    // 有声那路正常出字幕 —— 这就是用户要的「自适应」（无需手动切声源）。
    let mut cfg = cfg;
    if cfg.sources.is_empty() {
        cfg.sources = vec![SourceSpec::new(kind)];
    }

    // 运行时（卓易通容器 vs 原生）
    let runtime = if runtime == 1 {
        crate::config::AndroidRuntime::ZhuoYiTong
    } else {
        crate::config::AndroidRuntime::Native
    };

    let session = Box::new(FfiSession {
        config: AppConfig::default(),
        engine: Mutex::new(match SessionEngine::new(&cfg) {
            Ok(e) => e,
            Err(_) => return std::ptr::null_mut(),
        }),
        // **立即建立采集器** —— UI 在 start 之前就要查询采集能力
        // （`translator_session_capture_capability`）。
        // 采集器建好后接收端还在这里，start 时才移交给引擎。
        capture: Mutex::new(Vec::new()),
        summary: Mutex::new(None),
        writer: Mutex::new(None),
        events: Mutex::new(Vec::new()),
        seq: Mutex::new(0),
        transcript_path: Mutex::new(None),
        runtime: if runtime == crate::config::AndroidRuntime::ZhuoYiTong {
            1
        } else {
            0
        },
        expected_sources: Mutex::new(Vec::new()),
        stop_done_rx: Mutex::new(None),
    });

    // 建采集器 —— **按 cfg.sources 建多个**（内放 + 麦克风可同时存在）。
    // 失败**不致命**：UI 需要能从 `capture_capability` 拿到
    // 「不可用 + 原因」，而不是 create 直接返回 null。
    {
        for spec in &cfg.sources {
            crate::alog!("建采集器：{:?}", spec.kind);
            let cap_src = match spec.kind {
                SourceKind::SystemAudio => crate::config::CaptureSource::SystemAudio,
                SourceKind::Microphone => crate::config::CaptureSource::Microphone,
            };
            match CaptureHandle::create(cap_src, runtime) {
                Ok(c) => {
                    let label = c.source_label();
                    if let Ok(mut v) = session.capture.lock() {
                        v.push(c);
                    }
                    if let Ok(mut e) = session.expected_sources.lock() {
                        e.push(label);
                    }
                }
                Err(e) => {
                    // 建不起来也要记住声源 —— 否则 UI 显示「无声源」，
                    // 用户不知道是选了麦克风还是程序坏了。
                    eprintln!("[translator_core] 采集器创建失败（{:?}）：{e}", spec.kind);
                    // 标签按运行时区分：Android（Kotlin 注入链路）不是 WASAPI。
                    let android_inject = cfg!(feature = "capture-android")
                        || !cfg!(feature = "capture-desktop");
                    let label = match spec.kind {
                        SourceKind::SystemAudio => {
                            if android_inject {
                                "系统内放（AudioPlaybackCapture · Kotlin 注入）".to_string()
                            } else {
                                "系统内放（WASAPI loopback）".to_string()
                            }
                        }
                        SourceKind::Microphone => "麦克风".to_string(),
                    };
                    if let Ok(mut e) = session.expected_sources.lock() {
                        e.push(label);
                    }
                }
            }
        }
    }

    Box::into_raw(session) as *mut c_void
}

/// 释放会话。内部先 stop（冲刷字幕）再释放。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_free(handle: *mut c_void) {
    ffi_guard_void!({
        if handle.is_null() {
            return;
        }
        let session = Box::from_raw(handle as *mut FfiSession);
        // 幂等：先确保停止 worker 跑起来，再等它完成（15s 上限）。
        // ⚠️ 超时**不能 drop**：FfiSession 的 Drop → SessionEngine::drop
        // 还会 join 两个线程，卡死时会把 UI/退出流程一起挂住。
        // 社区标准做法：放弃等待就 leak 会话（进程即将退出，OS 回收）。
        let mut session = session;
        let done = ensure_stop_worker(&mut session)
            .ok()
            .and_then(|_| {
                session
                    .stop_done_rx
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|rx| rx.recv_timeout(std::time::Duration::from_secs(15))))
            });
        match done {
            Some(Ok(())) | Some(Err(std::sync::mpsc::RecvTimeoutError::Disconnected)) => {
                drop(session); // worker 已完成 → Drop 里的 join 立即返回，安全
            }
            _ => {
                // 超时：leak 会话，绝不 drop（Drop 会 join 卡死）
                std::mem::forget(session);
                eprintln!("[session_free] 停止超时，leak 会话（进程即将退出）");
            }
        }
    })
}

/// 启动会话：移交音频接收端 → 起引擎 → 起采集。
///
/// **立即返回**：引擎加载（onnxruntime 初始化，数秒）在监督线程内完成，
/// UI 通过 `translator_session_status` 轮询状态。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_start(handle: *mut c_void) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &mut *(handle as *mut FfiSession);

        // 1) 每个采集器的接收端移交给引擎 —— 音频从此不经过 Dart
        //
        // 接收端只能取一次 —— 上一次会话结束后再 start 会取不到。
        // 这时**重建采集器**，让「停止 → 重新开始」能正常工作。
        // 旧实现直接返回 AlreadyRunning，用户点了开始却没反应。
        let sources: Vec<(Receiver<PcmChunk>, SourceSpec)> = {
            let mut cap = match session.capture.lock() {
                Ok(c) => c,
                Err(_) => return FfiError::Internal as i32,
            };
            let cfg = match session.engine.lock() {
                Ok(e) => e.config().clone(),
                Err(_) => return FfiError::Internal as i32,
            };
            let runtime = crate::config::AndroidRuntime::Native;
            let runtime = if session.runtime == 1 {
                crate::config::AndroidRuntime::ZhuoYiTong
            } else {
                runtime
            };

            let mut out: Vec<(Receiver<PcmChunk>, SourceSpec)> = Vec::new();
            let mut all_taken = true;
            for c in cap.iter() {
                match c.take_receiver() {
                    Some(rx) => {
                        // 按**采集器自身的声源类型**去 cfg.sources 里找对应 spec
                        // （含用户自定义的说话人标签）。
                        //
                        // ⚠️ 不能用位置下标 `cfg.sources.get(i)`：采集器列表
                        // 只含创建成功的，某路失败就会错位 →
                        // 麦克风的音频被挂上内放的说话人标签（对方/我 全乱）。
                        let spec = cfg
                            .sources
                            .iter()
                            .find(|s| {
                                s.kind == SourceKind::from(c.source())
                            })
                            .cloned()
                            .unwrap_or_else(|| {
                                SourceSpec::new(SourceKind::from(c.source()))
                            });
                        out.push((rx, spec));
                    }
                    None => {
                        // 这个源用过了 —— 整批重建（半新半旧会让
                        // 多声源的时间轴对不齐）
                        all_taken = false;
                        break;
                    }
                }
            }

            if !all_taken {
                // 重建：先停掉旧采集器，再全部换新
                for c in cap.iter() {
                    let _ = c.stop();
                }
                cap.clear();
                out.clear();
                for spec in &cfg.sources {
                    let src = match spec.kind {
                        SourceKind::SystemAudio => {
                            crate::config::CaptureSource::SystemAudio
                        }
                        SourceKind::Microphone => {
                            crate::config::CaptureSource::Microphone
                        }
                    };
                    match CaptureHandle::create(src, runtime) {
                        Ok(c) => {
                            let Some(rx) = c.take_receiver() else {
                                return FfiError::Internal as i32;
                            };
                            out.push((rx, spec.clone()));
                            cap.push(c);
                        }
                        Err(e) => {
                            eprintln!(
                                "[translator_core] 重建采集器失败（{:?}）：{e}",
                                spec.kind
                            );
                            return FfiError::NoAudioDevice as i32;
                        }
                    }
                }
            }
            out
        };
        if sources.is_empty() {
            return FfiError::InvalidArg as i32;
        }

        eprintln!("[start_session] receivers 已移交 ({} 源)，准备起引擎", sources.len());

        // 2) 清掉上一场的摘要 —— 否则重新开始后导出可能显示旧数据
        if let Ok(mut s) = session.summary.lock() {
            *s = None;
        }

        // 3) 转写盘（可选）
        let sink: Box<dyn crate::pipeline::TranscriptSink> =
            match build_sink(session) {
                Some(s) => s,
                None => Box::new(crate::pipeline::NoopSink),
            };

        // 4) 起引擎
        {
            let mut engine = match session.engine.lock() {
                Ok(e) => e,
                Err(_) => return FfiError::Internal as i32,
            };
            if engine.is_running() {
                return FfiError::AlreadyRunning as i32;
            }
            eprintln!("[start_session] 调 engine.start_multi（内部等监督线程建后端）...");
            engine.start_multi(sources, sink);
            eprintln!("[start_session] start_multi 返回");
        }

        // 5) 起采集
        {
            let cap = match session.capture.lock() {
                Ok(c) => c,
                Err(_) => return FfiError::Internal as i32,
            };
            eprintln!("[start_session] 开始起采集 ({} 个)...", cap.len());
            for c in cap.iter() {
                if let Err(e) = c.start() {
                    eprintln!("[translator_core] 采集启动失败: {e}");
                    return FfiError::NoAudioDevice as i32;
                }
            }
        }
        eprintln!("[start_session] 全部完成，返回 Ok");
        FfiError::Ok as i32
    })
}

/// 运行时启停某一路采集（2026-10-28 用户需求「转写过程中实时切换采集源」）。
///
/// `kind`：0 = 系统内放，1 = 麦克风。
/// `enabled`：1 = 启用，0 = 停用。
///
/// ## 为什么按**声源类型**而不是下标
///
/// `session.capture` 只含**创建成功**的采集器 —— 某路建不起来时
/// （无麦克风 / 投屏未授权），列表里就少一个，位置下标与 UI 的
/// kind 完全对不上。用类型匹配永远正确（与 `capture_index` 同理）。
///
/// ## 为什么停用后还能再启用
///
/// `CaptureHandle::start/stop` 支持重复调用：
/// - `start()` 每次都**重新打开音频设备**（WASAPI loopback / mic），
///   且用的还是内部那个 `chunk_tx` —— 接收端已交给引擎，
///   故重启后音频**继续流向同一个引擎**，字幕时间轴不断。
/// - `stop()` 停采集线程并 join。
///
/// ## 平台差异
///
/// - **桌面端**：本函数直接生效（数据由 Rust 自己的采集线程产出）。
/// - **Android**：采集数据由 Kotlin 的 `AudioCaptureService` 推入
///   （Rust 侧是 `ExternalPushCapture`，其 start/stop 为空实现）。
///   故 Android 上本函数是**无害的 no-op**，真正的启停在 Kotlin 侧
///   （见 `AudioCaptureService.setSourceEnabled`）。
///   Dart 侧按平台分派，两端行为对用户一致。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_set_source_enabled(
    handle: *mut c_void,
    kind: i32,
    enabled: i32,
) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let want = match kind {
            0 => crate::config::CaptureSource::SystemAudio,
            1 => crate::config::CaptureSource::Microphone,
            _ => return FfiError::InvalidArg as i32,
        };
        let session = &*(handle as *const FfiSession);
        let cap = match session.capture.lock() {
            Ok(c) => c,
            Err(_) => return FfiError::Internal as i32,
        };
        let mut hit = false;
        for c in cap.iter() {
            if c.source() != want {
                continue;
            }
            hit = true;
            let r = if enabled != 0 { c.start() } else { c.stop() };
            if let Err(e) = r {
                eprintln!(
                    "[translator_core] 切换声源失败（{want:?}，enabled={enabled}）：{e}"
                );
                return FfiError::NoAudioDevice as i32;
            }
        }
        if !hit {
            // 本会话没有该类型的采集器（创建时就失败了）
            eprintln!("[translator_core] 切换声源失败：会话内没有 {want:?} 采集器");
            return FfiError::InvalidArg as i32;
        }
        FfiError::Ok as i32
    })
}

/// 暂停 / 恢复转写。
///
/// `paused`：1 = 暂停，0 = 恢复。
///
/// **暂停保留会话与音频接收端**，恢复后接着转 —— 与
/// `translator_session_stop`（真结束）不同。
///
/// 暂停期间引擎不消费音频，但**在途译文继续处理**；
/// 恢复时暂停期间积压的音频会被丢弃（用户早就不想录了）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_pause(
    handle: *mut c_void,
    paused: i32,
) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let engine = match session.engine.lock() {
            Ok(e) => e,
            Err(_) => return FfiError::Internal as i32,
        };
        if paused != 0 {
            engine.pause();
        } else {
            engine.resume();
        }
        FfiError::Ok as i32
    })
}

/// 停止会话（同步冲刷最后一句的译文）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_stop(handle: *mut c_void) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &mut *(handle as *mut FfiSession);
        // 3 秒超时（**阻塞已迁到后台 isolate，可以安心等**）：
        // 退出时 Dart 侧把这个同步调用放在独立 isolate 里执行，
        // UI isolate 不再被冻结，所以这里回到 3 秒，让最后半句的
        // 冲刷 + 落盘真正完成 —— 保存完整与界面不卡两者兼得。
        match stop_session(session, std::time::Duration::from_secs(3)) {
            Ok(_) => FfiError::Ok as i32,
            Err(e) => {
                eprintln!("[translator_core] 停止失败: {e}");
                FfiError::Internal as i32
            }
        }
    })
}

/// 确保「停止 worker」已启动（幂等）。
///
/// worker 做真正的停采集 + join 引擎/MT 线程，完成时经 channel 报信。
/// 用裸指针访问 session —— FFI 生命周期内 session 有效（Box 分配，
/// free 之前不释放）；字段各自有 Mutex，无数据竞争。
fn ensure_stop_worker(session: &mut FfiSession) -> Result<()> {
    {
        let g = session
            .stop_done_rx
            .lock()
            .map_err(|e| Error::Internal(format!("stop 信号锁被污染: {e}")))?;
        if g.is_some() {
            return Ok(()); // 已起过
        }
    }
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    if let Ok(mut g) = session.stop_done_rx.lock() {
        *g = Some(rx);
    }
    let ptr = session as *mut FfiSession;
    // 裸指针默认不是 Send —— 包一层 newtype 声明「这个指针跨线程使用是
    // 安全的」：worker 只通过字段级 Mutex 访问 session（无 &mut 冲突），
    // 且 session 在 free 之前一定有效（Box 分配，free 等 worker 报信）。
    struct SendPtr(*mut FfiSession);
    unsafe impl Send for SendPtr {}
    let sp = SendPtr(ptr);
    std::thread::Builder::new()
        .name("session-stop".into())
        .spawn(move || {
            // 先整体接住 sp，再取字段 —— 否则 Rust 2021 的 disjoint
            // closure capture 会直接捕获 `sp.0`（裸指针字段），绕过
            // SendPtr 的 unsafe impl Send。
            let sp = sp;
            let session = unsafe { &mut *sp.0 };
            // 1) 先停采集 —— 不再产生新音频
            if let Ok(cap) = session.capture.lock() {
                for c in cap.iter() {
                    let _ = c.stop();
                }
            }
            // 2) 停引擎（join 两线程；可能数秒，卡死也不拖累 UI）
            let summary = match session.engine.lock() {
                Ok(mut e) => e.stop(),
                Err(_) => Err(Error::Internal("引擎锁被污染".into())),
            };
            if let Ok(summary) = summary {
                if let Ok(mut s) = session.summary.lock() {
                    *s = Some(summary);
                }
            }
            // 3) 报信（不管成败 —— 调用方还要继续走 free/退出流程）
            let _ = tx.send(());
            eprintln!("[session-stop] 停止 worker 完成");
        })
        .map_err(|e| Error::Internal(format!("停止线程创建失败: {e}")))?;
    Ok(())
}

/// 停止会话（异步）：起/复用停止 worker，FFI 线程只等 [wait] 超时。
///
/// ## 为什么异步（2026-10-23 用户实测「未响应」根因）
///
/// join 可能永久阻塞（MT 卡在 translate / 引擎卡在推理）。同步 join 会把
/// Dart UI isolate 一起卡死 —— 用户截图「Translator (未响应)」。
/// 社区标准做法（SO 高赞 + highlit）：worker 完成时 send 报信，
/// 调用方 recv_timeout，超时就放弃 join（leak 线程，它最终自己退出）。
fn stop_session(session: &mut FfiSession, wait: std::time::Duration) -> Result<()> {
    ensure_stop_worker(session)?;
    if let Ok(g) = session.stop_done_rx.lock() {
        if let Some(rx) = g.as_ref() {
            let _ = rx.recv_timeout(wait);
        }
    }
    Ok(())
}

/// 取走自上次调用以来的增量字幕事件（JSON）。
///
/// 字符串由 Rust 分配，调用方必须用 `translator_free_string` 释放。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_take_events(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let events = {
            let mut engine = match session.engine.lock() {
                Ok(e) => e,
                Err(_) => return std::ptr::null_mut(),
            };
            engine.take_events()
        };
        to_c_string(&events_to_json(&events))
    })
}

/// 取 partial 预览文本（JSON 字符串）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_partial(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let snap = match session.engine.lock() {
            Ok(e) => e.snapshot(),
            Err(_) => return std::ptr::null_mut(),
        };
        let partial = snap.partial.unwrap_or_default();
        to_c_string(&partial)
    })
}

/// 取流式译文（英文预览对应的中文，随 partial 实时生长）。
///
/// 与 `translator_session_partial` 配对：partial 是 ASR 英文流，
/// partial_zh 是 MT 中文流。UI 同帧读取即可双语刷新。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_partial_zh(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let snap = match session.engine.lock() {
            Ok(e) => e.snapshot(),
            Err(_) => return std::ptr::null_mut(),
        };
        let zh = snap.partial_zh.unwrap_or_default();
        to_c_string(&zh)
    })
}

/// 读取会话状态。
///
/// # Safety
/// `out` 必须指向可写的 `FfiSessionStatus`（至少  sizeof(FfiSessionStatus) 字节）。
#[no_mangle]
pub unsafe extern "C" fn translator_session_status(
    handle: *mut c_void,
    out: *mut FfiSessionStatus,
) -> i32 {
    ffi_guard!({
        if handle.is_null() || out.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let snap = match session.engine.lock() {
            Ok(e) => e.snapshot(),
            Err(_) => return FfiError::Internal as i32,
        };
        let summary = session.summary.lock().ok().and_then(|s| s.clone());

        let status = FfiSessionStatus {
            state: snap.state as i32,
            asr_ready: (snap.state == SessionState::Running) as i32,
            // MT 可用性**如实反映后端**（阶段1 只装 ASR → 0），
            // 不再等同于「会话在跑」—— 否则 UI 会误以为译文在路上。
            //
            // 语义：会话未启动时也返回真实可用性（引擎启动前已预判），
            // 让 UI 在「还没点开始」时就能显示「翻译引擎未启用」。
            mt_ready: snap.mt_available as i32,
            error_code: if snap.error.is_some() {
                FfiError::EngineInit as i32
            } else {
                FfiError::Ok as i32
            },
            last_seq: snap.last_seq,
            line_count: summary.as_ref().map(|s| s.lines.len() as u64).unwrap_or(0),
            dropped_frames: 0,
            mt_dropped: snap.stats.mt_dropped,
            transcript_written: summary
                .as_ref()
                .map(|s| s.transcript_written)
                .unwrap_or(0),
            transcript_failures: summary
                .as_ref()
                .map(|s| s.transcript_failures)
                .unwrap_or(0),
        };
        std::ptr::write(out, status);
        FfiError::Ok as i32
    })
}

/// 取错误详情（如「模型未安装」「词库第 12 行解析失败」）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_error_desc(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let snap = match session.engine.lock() {
            Ok(e) => e.snapshot(),
            Err(_) => return std::ptr::null_mut(),
        };
        let msg = snap.error.unwrap_or_default();
        to_c_string(&msg)
    })
}

/// 导出字幕。`format`：0=SRT 1=VTT 2=TXT。
///
/// **从转写盘全量读**，而不是从内存窗口 ——
/// 运行中导出也能拿到完整历史（含已被窗口丢弃的行），
/// 且不必等会话结束。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_export(
    handle: *mut c_void,
    format: i32,
) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);

        // 优先：转写盘（运行中也完整）
        let path = match session.transcript_path.lock() {
            Ok(p) => p.clone(),
            Err(_) => None,
        };
        if let Some(p) = path {
            if let Ok(replay) = crate::transcript::replay(&p) {
                if !replay.lines.is_empty() {
                    return to_c_string(&crate::transcript::format_replay(
                        &replay.lines,
                        format,
                    ));
                }
            }
        }

        // 回退：内存窗口（未启用转写盘时）
        let summary = match session.summary.lock() {
            Ok(s) => s.clone(),
            Err(_) => return std::ptr::null_mut(),
        };
        let Some(summary) = summary else {
            return to_c_string("");
        };
        let text = match format {
            0 => summary.srt,
            1 => summary.vtt,
            2 => summary.txt,
            _ => summary.txt,
        };
        to_c_string(&text)
    })
}

/// 导入词库文件。返回词条数，负数为错误码。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`；`path` 为有效 UTF-8 C 字符串。
#[no_mangle]
pub unsafe extern "C" fn translator_glossary_load(handle: *mut c_void, path: *const c_char) -> i32 {
    ffi_guard!({
        if handle.is_null() || path.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let p = match std::ffi::CStr::from_ptr(path).to_str() {
            Ok(p) => p,
            Err(_) => return FfiError::InvalidArg as i32,
        };
        let session = &*(handle as *const FfiSession);
        let engine = match session.engine.lock() {
            Ok(e) => e,
            Err(_) => return FfiError::Internal as i32,
        };
        match engine.load_glossary_file(&PathBuf::from(p)) {
            Ok(n) => n as i32,
            Err(e) => {
                eprintln!("[translator_core] 词库导入失败: {e}");
                FfiError::InvalidArg as i32
            }
        }
    })
}

/// 重载词库（磁盘改动后）。失败保留旧快照。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_glossary_reload(handle: *mut c_void) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let engine = match session.engine.lock() {
            Ok(e) => e,
            Err(_) => return FfiError::Internal as i32,
        };
        match engine.reload_glossary() {
            Ok(()) => FfiError::Ok as i32,
            Err(e) => {
                eprintln!("[translator_core] 词库重载失败: {e}");
                FfiError::InvalidArg as i32
            }
        }
    })
}

/// 当前生效的词条总数（含内置与用户导入，2026-10-25 词库页要显示真实状态）。
///
/// 返回负数 = 错误码（句柄非法等）；非负 = 词条数。
#[no_mangle]
pub unsafe extern "C" fn translator_glossary_count(handle: *mut c_void) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let engine = match session.engine.lock() {
            Ok(e) => e,
            Err(_) => return FfiError::Internal as i32,
        };
        engine.glossary_entry_count() as i32
    })
}

/// 启动自检报告（JSON）。
///
/// 5 步：库加载 / ASR 模型 / MT 模型 / 词库 / 采集能力。
/// UI 用它来诚实展示「哪一步没就绪」，而不是笼统地说「启动失败」。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_selfcheck_json(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let cfg = {
            let engine = match session.engine.lock() {
                Ok(e) => e,
                Err(_) => return std::ptr::null_mut(),
            };
            engine.config().clone()
        };

        let asr = crate::modelspec::probe_asr_dir(&cfg.asr_model_dir);
        let mt = crate::modelspec::probe_mt_dir(&cfg.mt_model_dir);
        let capture = {
            let cap = session.capture.lock();
            match cap {
                Ok(cs) => {
                    if cs.is_empty() {
                        None
                    } else {
                        let kinds: Vec<String> = cs
                            .iter()
                            .map(|c| match c.capability() {
                                crate::config::CaptureCapability::SystemAudio => "内放".to_string(),
                                _ => "麦克风".to_string(),
                            })
                            .collect();
                        Some(kinds.join(" + "))
                    }
                }
                Err(_) => None,
            }
        };

        let mut s = String::from("{\"steps\":[");
        let mut first = true;
        let mut push = |name: &str, ok: bool, detail: &str| {
            if !first {
                s.push(',');
            }
            first = false;
            s.push_str(&format!(
                "{{\"name\":{},\"ok\":{},\"detail\":{}}}",
                esc(name),
                ok,
                esc(detail)
            ));
        };

        push("lib", true, "translator_core 已加载");
        push(
            "asr",
            asr.is_ok(),
            &match &asr {
                Ok(_) => "模型目录就绪".to_string(),
                Err(e) => e.to_string(),
            },
        );
        push(
            "mt",
            mt.is_ok(),
            &match &mt {
                Ok(_) => "权重目录就绪".to_string(),
                Err(e) => e.to_string(),
            },
        );
        let entries = {
            let engine = session.engine.lock();
            match engine {
                Ok(e) => e.glossary_entry_count(),
                Err(_) => 0,
            }
        };
        push("glossary", true, &format!("{entries} 个词条"));
        let cap_desc = capture.clone().unwrap_or_else(|| "采集器未创建".to_string());
        push(
            "capture",
            capture.is_some(),
            &cap_desc,
        );
        s.push_str("]}");
        to_c_string(&s)
    })
}

/// 诊断用：取会话的采集/转写统计 JSON。
///
/// ## 为什么需要它
///
/// 2026-10-23 用户实测「一直说 Can you read this，只有零星识别」。
/// 排查时发现 `FfiSessionStatus.dropped_frames` 被**硬编码为 0**，
/// `PipelineStats` 里也没有采集侧计数 —— 看不到
/// 「VAD 切了几段 / ASR 收到几段 / 几段识别为空」，
/// 就无法判断是采集丢数据、VAD 切太碎还是 ASR 识别失败。
///
/// 本接口把三者一次暴露，让这类「不完整」问题可定位而非靠猜。
#[no_mangle]
pub unsafe extern "C" fn translator_session_diag_json(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let snap = match session.engine.lock() {
            Ok(e) => e.snapshot(),
            Err(_) => return std::ptr::null_mut(),
        };
        let st = &snap.stats;
        let line_count = session
            .summary
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .map(|s| s.lines.len())
            .unwrap_or(0);
        // 采集侧统计：判断「麦克风到底采没采到声音」
        let (cap_rms, cap_max_rms, cap_rate, cap_channels, cap_chunks, cap_dropped) = {
            let cap = session.capture.lock();
            match cap {
                Ok(caps) => {
                    let mut rms = 0.0f32;
                    let mut max_rms = 0.0f32;
                    let mut cap_rate = 0u32;
                    let mut cap_channels = 0u32;
                    let mut chunks = 0u64;
                    let mut dropped = 0u64;
                    for c in caps.iter() {
                        if let Some(s) = c.capture_stats() {
                            rms = rms.max(s.rms);
                            max_rms = max_rms.max(s.max_rms);
                            cap_rate = s.native_rate;
                            cap_channels = s.native_channels;
                            chunks += s.chunks;
                            dropped += s.dropped;
                        }
                    }
                    (rms, max_rms, cap_rate, cap_channels, chunks, dropped)
                }
                Err(_) => (0.0, 0.0, 0, 0, 0, 0),
            }
        };
        let json = format!(
            "{{\"vad_segments\":{},\"asr_finals\":{},\"asr_empty\":{},\"mt_done\":{},\"mt_dropped\":{},\"mt_failed\":{},\"last_seq\":{},\"line_count\":{},\"capture_rms\":{:.6},\"capture_max_rms\":{:.6},\"native_rate\":{},\"native_channels\":{},\"capture_chunks\":{},\"capture_dropped\":{}}}",
            st.vad_segments,
            st.asr_finals,
            st.vad_segments.saturating_sub(st.asr_finals),
            st.mt_done,
            st.mt_dropped,
            st.mt_failed,
            snap.last_seq,
            line_count,
            cap_rms,
            cap_max_rms,
            cap_rate,
            cap_channels,
            cap_chunks,
            cap_dropped,
        );
        CString::new(json)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut())
    })
}

/// 导入模型目录（阶段一：只校验，不复制）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`；
/// `src_dir` 必须是有效的 C 字符串指针。
#[no_mangle]
pub unsafe extern "C" fn translator_model_import_dir(
    handle: *mut c_void,
    src_dir: *const c_char,
) -> i32 {
    ffi_guard!({
        if handle.is_null() || src_dir.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let dir = match std::ffi::CStr::from_ptr(src_dir).to_str() {
            Ok(p) => PathBuf::from(p),
            Err(_) => return FfiError::InvalidArg as i32,
        };
        if !dir.is_dir() {
            return FfiError::InvalidArg as i32;
        }
        // 阶段一：只校验目录里有没有可用的模型，不做复制
        let asr_ok = crate::modelspec::probe_asr_dir(&dir).is_ok()
            || crate::modelspec::probe_mt_dir(&dir).is_ok();
        if !asr_ok {
            return FfiError::ModelMissing as i32;
        }
        FfiError::Ok as i32
    })
}

/// 从转写盘按 seq 范围取行（JSON）。用于 UI 向上滚动回看历史。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_transcript_query(
    handle: *mut c_void,
    from_seq: u64,
    max_lines: u32,
) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let path = match session.transcript_path.lock() {
            Ok(p) => p.clone(),
            Err(_) => return std::ptr::null_mut(),
        };
        let Some(path) = path else {
            return to_c_string("{\"lines\":[],\"truncated\":false}");
        };
        match crate::transcript::query(&path, Some(from_seq), max_lines as usize) {
            Ok(r) => to_c_string(&crate::transcript::reader::to_json(&r)),
            Err(e) => {
                eprintln!("[translator_core] 读取转写盘失败: {e}");
                to_c_string("{\"lines\":[],\"truncated\":false}")
            }
        }
    })
}

/// 本场转写盘文件路径。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_transcript_path(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let path = match session.transcript_path.lock() {
            Ok(p) => p.clone(),
            Err(_) => return std::ptr::null_mut(),
        };
        match path {
            Some(p) => to_c_string(&p.display().to_string()),
            None => to_c_string(""),
        }
    })
}

/// 转写盘统计。
///
/// # Safety
/// `out` 必须指向可写的 `FfiTranscriptStats`。
#[no_mangle]
pub unsafe extern "C" fn translator_transcript_stats(
    handle: *mut c_void,
    out: *mut FfiTranscriptStats,
) -> i32 {
    ffi_guard!({
        if handle.is_null() || out.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let summary = session.summary.lock().ok().and_then(|s| s.clone());
        let snap = session.engine.lock().ok().map(|e| e.snapshot());
        std::ptr::write(
            out,
            FfiTranscriptStats {
                written: summary.as_ref().map(|s| s.transcript_written).unwrap_or(0),
                failures: summary
                    .as_ref()
                    .map(|s| s.transcript_failures)
                    .unwrap_or(0),
                last_seq: snap.map(|s| s.last_seq).unwrap_or(0),
            },
        );
        FfiError::Ok as i32
    })
}

/// 默认转写盘目录（桌面 `translate results`）。UI 用来展示与预填。
#[no_mangle]
pub extern "C" fn translator_transcript_dir() -> *mut c_char {
    ffi_guard_ptr!({
        match crate::platform::userdir::default_transcript_dir() {
            Some(d) => to_c_string(&d.display().to_string()),
            None => to_c_string(""),
        }
    })
}

/// 实时更新转写参数（2026-10-25：转写中动态调整，无需重启会话）。
///
/// `cfg_json` 形如 `{"asr_threads":8,"hangover_ms":600,"offline_refine":true}`，
/// **缺省项保持不变**。
///
/// ## 生效时机
/// - `hangover_ms` / `offline_refine`：**下一句立即生效**（零重建）
/// - `asr_threads`：**下一句边界**热切换 recognizer。sherpa 的线程数在
///   recognizer 创建时定死，改它必须重建；重建后新 session 无左上下文，
///   切换后第一句段首质量可能短暂下降（本项目实测过的坑，UI 需提示）。
///
/// 返回 `FfiError::Ok`(0) = 已应用；`InvalidArg` = 句柄或 JSON 非法。
#[no_mangle]
pub unsafe extern "C" fn translator_session_update_live(
    handle: *mut c_void,
    cfg_json: *const c_char,
) -> i32 {
    ffi_guard!({
        if handle.is_null() || cfg_json.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let json = match std::ffi::CStr::from_ptr(cfg_json).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return FfiError::InvalidArg as i32,
        };
        // 本项目不用 serde_json，用词库模块自带的小型 JSON 解析器
        //（零依赖、离线硬约束）。
        let v = match crate::glossary::parse::json::parse_value(&json) {
            Ok(x) => x,
            Err(_) => return FfiError::InvalidArg as i32,
        };
        let session = &*(handle as *const FfiSession);
        let engine = match session.engine.lock() {
            Ok(g) => g,
            Err(_) => return FfiError::Internal as i32,
        };
        let threads = v
            .get("asr_threads")
            .and_then(|x| x.as_i64())
            .map(|x| x as usize);
        let hangover = v
            .get("hangover_ms")
            .and_then(|x| x.as_i64())
            .map(|x| x as u64);
        let refine = v.get("offline_refine").and_then(|x| x.as_bool());
        let ver = engine.live().update(threads, hangover, refine);
        eprintln!(
            "[live] FFI 更新: threads={threads:?} hangover={hangover:?} refine={refine:?} → v{ver}"
        );
        FfiError::Ok as i32
    })
}

/// 借用采集句柄（**不可释放**）。
///
/// 只给 Android 用：Kotlin 的 `AudioRecord` 是生产者，要往这个句柄里推音频。
/// 句柄的**所有权属于会话** —— 释放会话即可，**不要**再调
/// `translator_free_capture`。
///
/// ⚠️ 会话 `start` 时若接收端已耗尽会**整批重建**采集器（L438 分支），
/// 重建后本函数此前返回的指针**失效**。跨重建场景请改用
/// [`translator_session_push_chunk`]（按索引即时查表，天然免疫悬垂）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_capture_handle(handle: *mut c_void) -> *mut c_void {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        // 借用：不转移所有权。返回的指针仅在会话存活期间有效。
        session
            .capture
            .lock()
            .ok()
            .and_then(|c| c.first().map(|c| c as *const CaptureHandle as *mut c_void))
            .unwrap_or(std::ptr::null_mut())
    })
}

/// 按声源索引推入音频（**Android 推荐路径**，规避句柄重建悬垂）。
///
/// 与 [`translator_session_capture_handle`] + `translator_push_chunk`
/// 等价，但每次即时在会话的采集器表上按索引查表 —— 会话 `start`
/// 重建采集器后**依旧有效**，Kotlin 只需持有会话指针与索引，
/// 不需要跟踪句柄代际。
///
/// `source_index` 与创建会话时 JSON 的 `sources` 数组下标一致
/// （重建路径按同一 `cfg.sources` 顺序重建，索引稳定）。
///
/// 返回 `FfiError` 码；队列满返回 `Internal`（调用方必须丢批，
/// 绝不重试阻塞 —— DESIGN §6.2）。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`；`samples` 指向
/// `count` 个有效 f32（16 kHz mono）。
#[no_mangle]
pub unsafe extern "C" fn translator_session_push_chunk(
    handle: *mut c_void,
    source_index: usize,
    samples: *const f32,
    count: usize,
) -> i32 {
    ffi_guard!({
        if handle.is_null() || samples.is_null() || count == 0 {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let cap = match session.capture.lock() {
            Ok(c) => c,
            Err(_) => return FfiError::Internal as i32,
        };
        let Some(h) = cap.get(source_index) else {
            return FfiError::InvalidArg as i32;
        };
        // 拷贝后再送：调用方（JNI）缓冲返回后立即复用
        let data = std::slice::from_raw_parts(samples, count).to_vec();
        match h.push_samples(data) {
            Ok(()) => FfiError::Ok as i32,
            Err(e) => FfiError::from(&e) as i32,
        }
    })
}

/// 查询某类声源在会话采集器列表中的**下标**（Android 注入链路用）。
///
/// ## 为什么需要这个
///
/// 会话的采集器列表**只含创建成功的**采集器（创建失败不致命，
/// 只记入 `expected_sources`）。故它的下标与创建时的
/// `cfg.sources` 下标**可能错位** —— 例如卓易通容器内
/// 内放采集器建不起来（MediaProjection 不可用），列表里就只剩
/// 麦克风那一路，它在下标 0，而 `cfg.sources` 里内放才是 0。
///
/// Kotlin 侧必须知道「我这一路音频该推进哪个下标」，
/// 否则会推到错的源上（内放/麦克风标签错乱，或推给空队列丢音频）。
///
/// `kind`：0 = 系统内放，1 = 麦克风。
/// 返回下标；该类声源不存在时返回 -1。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_capture_index(
    handle: *mut c_void,
    kind: i32,
) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return -1;
        }
        let session = &*(handle as *const FfiSession);
        let want = match kind {
            0 => crate::config::CaptureSource::SystemAudio,
            1 => crate::config::CaptureSource::Microphone,
            _ => return -1,
        };
        // 按**声源类型**查采集器下标，而不是按位置反推。
        //
        // ⚠️ 旧实现是「按 cfg.sources 顺序跳过创建失败的」：
        //   遍历 cfg_sources[i]，同时 cap_idx 递增，遇到 kind 匹配就返回 cap_idx。
        //   但当某路创建失败时（内放建不起来 → 列表里只剩麦克风在下标 0），
        //   cfg_sources[0]=内放 会被匹配到 cap[0]=麦克风 → **张冠李戴**：
        //   麦克风的音频被当成内放推，字幕说话人标签（对方/我）全错。
        //
        // 现在每个采集器自带 source 类型（CaptureHandle::source），
        // 按类型查永远正确，与列表位置无关。
        let cap = match session.capture.lock() {
            Ok(c) => c,
            Err(_) => return -1,
        };
        for (i, h) in cap.iter().enumerate() {
            if h.source() == want {
                return i as i32;
            }
        }
        -1
    })
}

/// 回传外部采集授权状态（Android：MediaProjection 授予/撤销时调用）。
///
/// 桌面实现无外部授权概念（自采），调用为 no-op，安全。
/// `granted`：非 0 = 内放授权已到位。
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_set_capture_granted(
    handle: *mut c_void,
    granted: i32,
) -> i32 {
    ffi_guard!({
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let session = &*(handle as *const FfiSession);
        let cap = match session.capture.lock() {
            Ok(c) => c,
            Err(_) => return FfiError::Internal as i32,
        };
        for h in cap.iter() {
            // CaptureHandle 暴露内部采集器的授权入口
            h.set_capture_granted(granted != 0);
        }
        FfiError::Ok as i32
    })
}

/// 查询会话的采集能力（JSON）。
///
/// ## 为什么需要这个导出
///
/// 桌面端的采集器由**会话自己拥有**（见 `FfiSession.capture`）。
/// UI 若另建独立采集句柄去探测能力，拿到的是另一套状态 ——
/// 那条路径在桌面端既没被调用过，也拿不到真实结果。
/// 本函数直接读会话里实际在用的采集器，UI 报的必然是真实能力。
///
/// ## 返回格式
///
/// ```json
/// {"kind":"systemAudio","reason":null,"sources":["内放","麦克风"]}
/// ```
/// - `kind`：`systemAudio` / `microphone` / `unavailable`
/// - `reason`：降级/不可用原因（可空）
/// - `sources`：会话实际持有的声源标签
///
/// # Safety
/// `handle` 必须来自 `translator_session_create`。
#[no_mangle]
pub unsafe extern "C" fn translator_session_capture_capability(handle: *mut c_void) -> *mut c_char {
    ffi_guard_ptr!({
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let session = &*(handle as *const FfiSession);
        let caps = match session.capture.lock() {
            Ok(c) => c,
            Err(_) => {
                return to_c_string(
                    "{\"kind\":\"unavailable\",\"reason\":\"采集器状态异常\",\"sources\":[]}",
                )
            }
        };

        // 各声源的能力：任一可用即整体可用；全不可用则取第一个原因。
        //
        // `sources` 用**期望声源**列表兜底：采集器建不起来时
        // （无音频设备 / 权限未授予）列表会是空的，
        // 但 UI 仍需知道用户选了哪个声源。
        let mut sources: Vec<String> = match session.expected_sources.lock() {
            Ok(v) => v.clone(),
            Err(_) => Vec::new(),
        };
        let mut best: Option<crate::config::CaptureCapability> = None;
        let mut first_reason: Option<String> = None;
        for c in caps.iter() {
            // 采集器存在时用它的真标签（比期望列表更准）
            if !sources.iter().any(|s| *s == c.source_label()) {
                sources.push(c.source_label());
            }
            let cap = c.capability();
            match &cap {
                crate::config::CaptureCapability::Unavailable { reason } => {
                    if first_reason.is_none() {
                        first_reason = Some(reason.clone());
                    }
                }
                crate::config::CaptureCapability::MicrophoneOnly { reason } => {
                    if first_reason.is_none() {
                        first_reason = Some(reason.clone());
                    }
                    if best.is_none() {
                        best = Some(cap.clone());
                    }
                }
                crate::config::CaptureCapability::SystemAudio => {
                    best = Some(cap.clone());
                }
            }
        }

        if sources.is_empty() {
            return to_c_string(
                "{\"kind\":\"unavailable\",\"reason\":\"会话未创建采集器\",\"sources\":[]}",
            );
        }

        let (kind, reason) = match best {
            Some(crate::config::CaptureCapability::SystemAudio) => ("systemAudio", String::new()),
            Some(crate::config::CaptureCapability::MicrophoneOnly { reason }) => ("microphone", reason),
            _ => (
                "unavailable",
                first_reason.unwrap_or_else(|| "无可用音频设备".to_string()),
            ),
        };

        to_c_string(&capture_capability_json(kind, &reason, &sources))
    })
}

/// 构造采集能力 JSON。
///
/// 抽成独立函数是为了能单测 —— 之前这里内联拼接，
/// 因对 `esc()` 重复加引号导致 JSON 非法（Dart 端 jsonDecode 直接抛），
/// 是被出厂验证门禁抓出来的。JSON 拼装有坑，必须有测试兜底。
fn capture_capability_json(kind: &str, reason: &str, sources: &[String]) -> String {
    // ⚠️ esc()（transcript::json_escape）**自带首尾引号**并转义内部引号，
    //    直接拼进 JSON 即可；再加一层 \" 会双重转义 → 非法 JSON。
    let src_json = sources
        .iter()
        .map(|s| esc(s))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"kind\":\"{}\",\"reason\":{},\"sources\":[{}]}}",
        kind,
        esc(reason),
        src_json
    )
}

// ── 内部辅助 ──────────────────────────────────────────
/// 建转写盘 sink（按配置里的 transcript_dir）
fn build_sink(session: &mut FfiSession) -> Option<Box<TranscriptWriter>> {
    let dir = {
        let engine = session.engine.lock().ok()?;
        engine.config().transcript_dir.clone()
    }?;
    match TranscriptWriter::create_in(&dir) {
        Ok(w) => {
            let p = w.path().to_path_buf();
            if let Ok(mut tp) = session.transcript_path.lock() {
                *tp = Some(p);
            }
            Some(Box::new(w))
        }
        Err(e) => {
            eprintln!("[translator_core] 转写盘创建失败（{}），降级为仅内存", e);
            None
        }
    }
}

/// 事件 → JSON
fn events_to_json(events: &[LineEvent]) -> String {
    let mut s = String::from("{\"events\":[");
    for (i, e) in events.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match e {
            LineEvent::Added {
                seq,
                source,
                start_ms,
                end_ms,
                forced_cut,
                speaker,
            } => {
                s.push_str(&format!(
                    "{{\"k\":\"add\",\"i\":{seq},\"src\":{},\"st\":{start_ms},\"et\":{end_ms},\"cut\":{forced_cut},\"spk\":{}}}",
                    esc(source),
                    match speaker {
                        Some(sp) => esc(sp),
                        None => "null".to_string(),
                    }
                ));
            }
            LineEvent::Filled {
                seq,
                target,
                needs_review,
            } => {
                s.push_str(&format!(
                    "{{\"k\":\"fill\",\"i\":{seq},\"tgt\":{},\"rev\":{needs_review}}}",
                    esc(target)
                ));
            }
            LineEvent::Revised { seq } => {
                s.push_str(&format!("{{\"k\":\"rev\",\"i\":{seq}}}"));
            }
        }
    }
    s.push_str("]}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PerformanceProfile;
    use crate::pipeline::wiring::BackendChoice;
    use crate::pipeline::PipelineConfig;

    #[test]
    fn capture_capability_json_is_valid_json() {
        // 之前这里对 esc() 重复加了引号 → 双重转义 → JSON 非法，
        // Dart 端 jsonDecode 直接抛异常。被出厂验证门禁抓出来。
        let json = capture_capability_json(
            "systemAudio",
            "",
            &["系统内放（WASAPI loopback）".to_string(), "麦克风".to_string()],
        );
        let parsed = crate::glossary::parse::json::parse_value(&json)
            .expect("采集能力 JSON 必须合法");
        assert_eq!(parsed.get("kind").and_then(|v| v.as_str()), Some("systemAudio"));
        assert_eq!(parsed.get("reason").and_then(|v| v.as_str()), Some(""));
        let srcs = parsed.get("sources").and_then(|v| v.as_array());
        assert_eq!(srcs.map(|a| a.len()), Some(2));
    }

    #[test]
    fn capture_capability_json_escapes_quotes_in_reason() {
        // 原因里带引号/反斜杠/换行时也必须产出合法 JSON
        let json = capture_capability_json(
            "unavailable",
            "设备名 \" speakers \\ broken\nline",
            &["麦克风".to_string()],
        );
        let parsed = crate::glossary::parse::json::parse_value(&json)
            .expect("含特殊字符时 JSON 仍须合法");
        let reason = parsed.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        assert!(reason.contains('"'));
        assert!(reason.contains('\\'));
        assert!(reason.contains('\n'));
    }

    #[test]
    fn capture_capability_json_with_empty_sources() {
        let json = capture_capability_json("unavailable", "会话未创建采集器", &[]);
        let parsed = crate::glossary::parse::json::parse_value(&json)
            .expect("空声源时 JSON 仍须合法");
        assert_eq!(
            parsed.get("kind").and_then(|v| v.as_str()),
            Some("unavailable")
        );
    }

    #[test]
    fn config_defaults_when_json_is_empty_object() {
        let cfg = config_from_json("{}").expect("空对象应走默认值");
        assert_eq!(cfg.backend, BackendChoice::Auto);
        assert_eq!(cfg.memory_window_lines, PipelineConfig::default().memory_window_lines);
        assert!(cfg.transcript_dir.is_none());
    }

    #[test]
    fn config_reads_all_fields() {
        let json = r#"{
            "asr_model_dir": "C:/models/asr",
            "mt_model_dir": "C:/models/mt",
            "glossary_paths": ["C:/g/a.tsv", "C:/g/b.csv"],
            "profile": "fast",
            "asr_threads": 4,
            "mt_threads": 3,
            "mt_accurate": true,
            "memory_window_lines": 500,
            "backend": "mock",
            "transcript_dir": "C:/out"
        }"#;
        let cfg = config_from_json(json).unwrap();
        assert_eq!(cfg.asr_model_dir, PathBuf::from("C:/models/asr"));
        assert_eq!(cfg.mt_model_dir, PathBuf::from("C:/models/mt"));
        assert_eq!(cfg.glossary_paths.len(), 2);
        assert_eq!(cfg.profile, PerformanceProfile::Fast);
        assert_eq!(cfg.asr_threads, 4);
        assert_eq!(cfg.mt_threads, 3);
        assert!(cfg.mt_accurate);
        assert_eq!(cfg.memory_window_lines, 500);
        assert_eq!(cfg.backend, BackendChoice::Mock);
        assert_eq!(cfg.transcript_dir, Some(PathBuf::from("C:/out")));
    }

    #[test]
    fn config_ignores_unknown_keys() {
        // 新版本加字段不应让旧版 UI 崩 —— 未知的键直接忽略
        let json = r#"{"backend":"mock","future_field":{"a":1},"another":[1,2,3]}"#;
        let cfg = config_from_json(json).expect("未知键不应导致失败");
        assert_eq!(cfg.backend, BackendChoice::Mock);
    }

    #[test]
    fn config_treats_empty_transcript_dir_as_disabled() {
        // Dart 侧会把「用户没选目录」传成空串，不能被当成磁盘根目录
        let cfg = config_from_json(r#"{"transcript_dir":""}"#).unwrap();
        assert!(cfg.transcript_dir.is_none());
    }

    #[test]
    fn config_rejects_malformed_json() {
        assert!(config_from_json("{not json").is_err());
        assert!(config_from_json("").is_err());
    }

    #[test]
    fn config_clamps_zero_threads() {
        // 0 线程会让引擎起不来，必须夹到至少 1
        let cfg = config_from_json(r#"{"asr_threads":0,"mt_threads":0}"#).unwrap();
        assert_eq!(cfg.asr_threads, 1);
        assert_eq!(cfg.mt_threads, 1);
    }

    #[test]
    fn config_accepts_numbers_written_as_strings() {
        // 某些 JSON 生成器会把所有标量都写成字符串
        let cfg = config_from_json(r#"{"asr_threads":"6"}"#).unwrap();
        assert_eq!(cfg.asr_threads, 6);
    }

    #[test]
    fn events_json_covers_all_three_kinds() {
        let events = vec![
            LineEvent::Added {
                seq: 7,
                source: "we deploy \"K8s\"".into(),
                start_ms: 100,
                end_ms: 200,
                forced_cut: true,
                speaker: Some("对方".into()),
            },
            LineEvent::Filled {
                seq: 7,
                target: "我们部署 K8s".into(),
                needs_review: false,
            },
            LineEvent::Revised { seq: 7 },
        ];
        let j = events_to_json(&events);
        assert!(j.contains("\"k\":\"add\""));
        assert!(j.contains("\"k\":\"fill\""));
        assert!(j.contains("\"k\":\"rev\""));
        // 引号必须转义，否则 Dart 的 jsonDecode 会抛
        assert!(j.contains("\\\""), "{j}");
        // 且仍是合法 JSON：能被自己的解析器读回来
        let back = crate::glossary::parse::json::parse_value(&j).expect("应是合法 JSON");
        assert!(back.get("events").is_some());
    }

    #[test]
    fn empty_events_json_is_valid() {
        let j = events_to_json(&[]);
        assert_eq!(j, "{\"events\":[]}");
        assert!(crate::glossary::parse::json::parse_value(&j).is_ok());
    }

    #[test]
    fn format_replay_txt_is_readable_dialogue_record() {
        // 用户要的导出格式：带时间戳、说话人、原文、译文的对话记录
        let lines = vec![
            crate::transcript::TranscriptLine {
                seq: 0,
                source: "we deploy Kubernetes".into(),
                speaker: "对方".into(),
                target: Some("我们部署 Kubernetes".into()),
                start_ms: 1_500,
                end_ms: 2_000,
                needs_review: false,
                forced_cut: false,
            },
            crate::transcript::TranscriptLine {
                seq: 1,
                source: "yeah let's do it".into(),
                speaker: "我".into(),
                target: None,
                start_ms: 3_000,
                end_ms: 3_500,
                needs_review: true,
                forced_cut: false,
            },
        ];
        let txt = crate::transcript::format_replay(&lines, 2);
        assert!(txt.contains("[00:00:01] 对方"), "{txt}");
        assert!(txt.contains("  EN: we deploy Kubernetes"), "{txt}");
        assert!(txt.contains("  ZH: 我们部署 Kubernetes"), "{txt}");
        assert!(txt.contains("[00:00:03] 我"), "{txt}");
        assert!(txt.contains("（翻译中）"), "未译行应有占位: {txt}");
        assert!(txt.contains("术语待复核"), "待复核行应标注: {txt}");
    }

    #[test]
    fn format_replay_srt_and_vtt_are_valid() {
        let lines = vec![crate::transcript::TranscriptLine {
            seq: 0,
            source: "hello".into(),
            speaker: "对方".into(),
            target: Some("你好".into()),
            start_ms: 1_500,
            end_ms: 2_000,
            needs_review: false,
            forced_cut: false,
        }];
        let srt = crate::transcript::format_replay(&lines, 0);
        assert!(srt.starts_with("1\n"), "{srt}");
        assert!(srt.contains("00:00:01,500 --> 00:00:02,000"), "{srt}");
        let vtt = crate::transcript::format_replay(&lines, 1);
        assert!(vtt.starts_with("WEBVTT"), "{vtt}");
        assert!(vtt.contains("00:00:01.500 --> 00:00:02.000"), "{vtt}");
    }

    #[test]
    fn to_c_string_returns_null_on_nul() {
        // 含 NUL 的字符串不能变成 C 字符串 —— 必须返回 null 让调用方识别
        assert!(to_c_string("a\0b").is_null());
        assert!(!to_c_string("正常文本").is_null());
    }
}
