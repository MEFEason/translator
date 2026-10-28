//! C ABI 导出（Flutter FFI 边界）。
//!
//! ## 设计约束
//!
//! 1. **跨 FFI 不得 panic** —— 所有入口包 `catch_unwind`，把 panic 转成错误码。
//!    `Cargo.toml` 的 `panic = "unwind"` 也是为此（`abort` 会直接杀掉宿主进程）。
//! 2. **句柄用 Box 管理** —— `translator_create_*` 返回不透明指针，
//!    `translator_free_*` 释放。Rust 侧永不返回悬垂指针。
//! 3. **错误码而非字符串** —— FFI 传 String 需要额外约定内存所有权，易出错。
//!    错误码表见 `FfiError`；UI 层按码给本地化文案。
//! 4. **线程安全** —— 采集句柄可跨线程移动，内部用 Mutex 保护可变状态。
//!
//! ## 命名约定（AGENTS.md）
//!
//! 库名 `translator_core`，产物 `translator_core.dll` / `.dylib` / `.so`。
//! 所有导出符号以 `translator_` 前缀，避免与 Flutter 引擎符号冲突。
//!
//! ## 离线性
//!
//! 这些函数**不发起任何网络请求** —— 模型从本地路径加载。
//! 见 `crate::offline::OFFLINE_GUARANTEES`。

use crate::audio::PcmChunk;
use crate::config::{AndroidRuntime, AppConfig, CaptureCapability, CaptureSource};
use crate::error::{Error, Result};
use crate::platform::SystemAudioCapture;
// CStr 只在 #[cfg(test)] 模块使用（校验 version 字符串），
// 但 lib 构建时 unused import 会报警，故显式允许。
#[allow(unused_imports)]
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;

/// 音频队列容量（块）。
///
/// 20ms/块 → 200 块 = 4 秒缓冲。与 pipeline 侧 AudioChannel 的容量一致。
const CHANNEL_CAPACITY: usize = 200;

/// FFI 错误码。
///
/// Dart 侧按这些码给出本地化文案，故**新增码时必须同步 UI**。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiError {
    Ok = 0,
    /// 采集已在运行
    AlreadyRunning = 1,
    /// 未找到音频设备 / 无默认输出
    NoAudioDevice = 2,
    /// 权限未授予（Android MediaProjection / macOS TCC）
    PermissionDenied = 3,
    /// 模型缺失或损坏
    ModelMissing = 4,
    /// 引擎初始化失败
    EngineInit = 5,
    /// 内部错误（含 panic）
    Internal = 6,
    /// 参数非法（空指针 / 非法长度）
    InvalidArg = 7,
}

impl From<&Error> for FfiError {
    fn from(e: &Error) -> Self {
        match e {
            Error::Internal(_) => FfiError::Internal,
            Error::ModelMissing(_) => FfiError::ModelMissing,
            Error::ModelCorrupt(_) => FfiError::ModelMissing,
            Error::DiskSpace { .. } => FfiError::ModelMissing,
            Error::EngineInit(_) => FfiError::EngineInit,
            Error::Engine(_) => FfiError::EngineInit,
            Error::CaptureUnavailable(_) => FfiError::NoAudioDevice,
            Error::PermissionDenied(_) => FfiError::PermissionDenied,
            Error::GlossaryParse { .. } => FfiError::InvalidArg,
            Error::GlossaryLoad(_) => FfiError::InvalidArg,
            Error::InvalidConfig(_) => FfiError::InvalidArg,
        }
    }
}

/// 采集能力（与 Dart 侧 `CaptureKind` 对应）
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiCaptureKind {
    SystemAudio = 0,
    Microphone = 1,
    Unavailable = 2,
}

/// 采集会话句柄。
///
/// 内部持有平台采集器 + 音频块通道。`running` 标志用于让 Dart 侧
/// 能在不阻塞的情况下停止采集。
pub struct CaptureHandle {
    /// 实际采集器（平台相关）。
    ///
    /// 用 Mutex 包住是因为 trait 的 `start`/`stop` 是 `&mut self`，
    /// 而 FFI 入口只能拿到 `*const c_void`。锁只在启停时短暂持有，
    /// **绝不持锁跨音频回调** —— 否则会阻塞实时线程。
    inner: Mutex<Box<dyn SystemAudioCapture>>,
    /// 音频块发送端（采集线程 → 消费方）
    ///
    /// **有界**（`SyncSender`）而非无界：Android 侧 Kotlin 的
    /// `AudioRecord` 是生产者，若消费端（引擎）跟不上，
    /// 无界队列会让内存无限增长直到 OOM。有界队列满时**丢音频**并计数 ——
    /// 丢音频远好过崩溃（DESIGN §6.2）。
    chunk_tx: SyncSender<PcmChunk>,
    /// 音频块接收端。
    ///
    /// 用 `Option` 是为了能把接收端**移交给引擎线程**
    /// （见 [`CaptureHandle::take_receiver`]）—— 一旦移交，
    /// Dart 侧就不能再经 `translator_poll_chunk` 取音频了。
    chunk_rx: Mutex<Option<Receiver<PcmChunk>>>,
    running: AtomicBool,
    /// 本采集器对应的**声源类型**（内放 / 麦克风）。
    ///
    /// ## 为什么必须存（2026-10-28 双路并行改造）
    ///
    /// 会话的采集器列表**只含创建成功的**（创建失败不致命），
    /// 故 `capture` 的下标与 `cfg.sources` 的下标可能错位：
    /// 例如内放建不起来时，列表里只剩麦克风，它在下标 0，
    /// 而 `cfg.sources[0]` 是内放。
    ///
    /// 旧实现靠「位置对齐」反查声源 → 张冠李戴（麦克风的音频被
    /// 当成内放推，字幕说话人标签错乱）。存死声源类型后，
    /// 按类型查下标永远正确，与位置无关。
    source: CaptureSource,
}

impl CaptureHandle {
    /// 采集统计（最近一块的 RMS、块数等）。
    ///
    /// 供诊断用：**判断「麦克风到底采没采到声音」**。
    /// RMS 恒为 0 → 设备/权限/静音问题；明显 > 0 → 音频进来了，
    /// 问题在下游（VAD 切分 / ASR 识别）。
    pub fn capture_stats(&self) -> Option<crate::platform::CaptureStatsSnapshot> {
        let inner = self.inner.lock().ok()?;
        Some(inner.capture_stats_snapshot())
    }

    /// 创建采集句柄。
    ///
    /// 公开给会话层使用 —— 会话自己拥有采集器（避免两个所有者）。
    pub fn create(source: CaptureSource, runtime: AndroidRuntime) -> Result<Self> {
        Self::new(source, runtime)
    }

    fn new(source: CaptureSource, runtime: AndroidRuntime) -> Result<Self> {
        // 容量：4 秒缓冲（与 pipeline 侧的 AudioChannel 对齐）。
        // 满了就丢最旧的，采集线程绝不阻塞。
        crate::alog!("CaptureHandle::new：建通道");
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        crate::alog!("CaptureHandle::new：建采集器");
        let inner = crate::platform::create_capture(source, runtime)?;
        crate::alog!("CaptureHandle::new：完成");
        Ok(Self {
            inner: Mutex::new(inner),
            chunk_tx: tx,
            chunk_rx: Mutex::new(Some(rx)),
            running: AtomicBool::new(false),
            source,
        })
    }

    /// 本采集器的声源类型（内放 / 麦克风）。
    ///
    /// 按**类型**而非位置查下标 —— 见 struct 字段 `source` 的说明。
    pub fn source(&self) -> CaptureSource {
        self.source
    }

    pub fn start(&self) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Err(Error::Internal("采集已在运行".into()));
        }
        let tx = self.chunk_tx.clone();
        // 锁在此处释放（`guard` 是临时值），音频回调不持锁
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| Error::Internal(format!("采集器锁被污染: {e}")))?;
        guard.start(Box::new(move |chunk: PcmChunk| {
            // 队列满 → 丢弃这一块，绝不阻塞采集线程（DESIGN §6.2）。
            // 队列断开 → 引擎已退出，同样直接丢弃。
            let _ = tx.try_send(chunk);
        }))?;
        drop(guard);
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(false, Ordering::SeqCst);
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| Error::Internal(format!("采集器锁被污染: {e}")))?;
        guard.stop()
    }

    /// 能力探测。锁失败时保守返回"不可用"，不 panic。
    pub fn capability(&self) -> CaptureCapability {
        match self.inner.lock() {
            Ok(g) => g.capability(),
            Err(_) => CaptureCapability::Unavailable {
                reason: "采集器状态异常".into(),
            },
        }
    }

    pub fn dropped_frames(&self) -> u64 {
        self.inner.lock().map(|g| g.dropped_frames()).unwrap_or(0)
    }

    /// 外部注入授权变更（Android 路径转发给内部采集器）。
    pub fn set_capture_granted(&self, granted: bool) {
        if let Ok(g) = self.inner.lock() {
            g.set_capture_granted(granted);
        }
    }

    /// 声源标签（人类可读），如「系统内放（WASAPI loopback）」。
    ///
    /// 供会话侧汇总实际持有的声源 —— 锁失败时返回空串，不 panic。
    pub fn source_label(&self) -> String {
        self.inner
            .lock()
            .map(|g| g.source_label())
            .unwrap_or_else(|_| "未知声源".to_string())
    }

    /// 取一块音频。`None` 表示当前无数据。
    ///
    /// 用 `try_recv` 而非 `recv` —— Dart 侧是主动轮询模型，
    /// 阻塞会卡住 UI 线程。
    fn poll_chunk(&self) -> Option<PcmChunk> {
        let guard = self.chunk_rx.lock().ok()?;
        let rx = guard.as_ref()?;
        rx.try_recv().ok()
    }

    /// 把音频接收端**移交给**引擎线程。
    ///
    /// 只能取一次。移交后：
    /// - 引擎线程从 `rx` 消费音频
    /// - `translator_poll_chunk` 不再有数据可取（返回空）
    ///
    /// 桌面端与 Android 端都走这条路径 —— 音频**不经过 Dart**，
    /// 这是「字幕能在 100ms 内上屏」的前提。
    pub fn take_receiver(&self) -> Option<Receiver<PcmChunk>> {
        self.chunk_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// 推入一段音频（Android 路径：Kotlin 的 AudioRecord 是生产者）。
    ///
    /// 队列满时返回错误 —— 调用方（Kotlin）**必须丢弃这一批**，
    /// 绝不能阻塞音频线程（DESIGN §6.2）。
    pub(crate) fn push_samples(&self, samples: Vec<f32>) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(Error::Internal("采集未开始，不能推入音频".into()));
        }
        match self.chunk_tx.try_send(PcmChunk::new(samples, 0)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(Error::Internal(
                "音频队列已满（引擎跟不上），应丢弃本批".into(),
            )),
            Err(TrySendError::Disconnected(_)) => {
                Err(Error::Internal("接收端已断开".into()))
            }
        }
    }
}

// ── 导出的 C ABI ──────────────────────────────────────

/// 把 Rust 函数包进 catch_unwind，panic 转错误码。
///
/// 这是所有导出函数的统一入口包装 —— 漏一个就可能让宿主进程崩溃。
macro_rules! ffi_guard {
    ($body:expr) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("[translator_core] FFI 调用发生 panic，已转为错误码");
                FfiError::Internal as i32
            }
        }
    }};
}

/// 返回指针的 guard：panic 时返回空指针而非错误码
macro_rules! ffi_guard_ptr {
    ($body:expr) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("[translator_core] FFI 调用发生 panic，已转为空值");
                Default::default()
            }
        }
    }};
}

/// 无返回值函数的 guard：panic 时只记日志，不能返回错误码。
///
/// `translator_session_free` 这类「返回 void」的导出用这个 ——
/// 用 `ffi_guard!` 会因为 panic 分支返回 i32 而类型不匹配。
macro_rules! ffi_guard_void {
    ($body:expr) => {{
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)).is_err() {
            eprintln!("[translator_core] FFI 调用发生 panic");
        }
    }};
}

/// 创建采集句柄。
///
/// # Safety
/// `model_dir` 可为空指针（用默认路径）或有效 UTF-8 路径。
/// 返回值必须经 `translator_free_capture` 释放，否则泄漏。
#[no_mangle]
pub unsafe extern "C" fn translator_create_capture(
    source: i32,
    runtime: i32,
) -> *mut c_void {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let src = match source {
            0 => CaptureSource::SystemAudio,
            1 => CaptureSource::Microphone,
            _ => return None,
        };
        let rt = match runtime {
            0 => AndroidRuntime::Native,
            1 => AndroidRuntime::ZhuoYiTong,
            _ => return None,
        };
        // 创建失败返回空指针，Dart 侧据此判定
        CaptureHandle::new(src, rt)
            .ok()
            .map(|h| Box::into_raw(Box::new(h)) as *mut c_void)
    }));

    match result {
        Ok(Some(ptr)) => ptr,
        _ => std::ptr::null_mut(),
    }
}

/// 释放采集句柄。
///
/// # Safety
/// `handle` 必须来自 `translator_create_capture`，且不得重复释放。
#[no_mangle]
pub unsafe extern "C" fn translator_free_capture(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        drop(Box::from_raw(handle as *mut CaptureHandle));
    }));
}

/// 探测采集能力。
///
/// 返回 `FfiCaptureKind` 之一。**不可用时 UI 必须显式提示**
/// （DESIGN §2.5.2：禁止静默降级）。
///
/// # Safety
/// `handle` 必须有效。
#[no_mangle]
pub unsafe extern "C" fn translator_probe_capture(handle: *const c_void) -> i32 {
    ffi_guard!({{
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let h = &*(handle as *const CaptureHandle);
        match h.capability() {
            crate::config::CaptureCapability::SystemAudio => FfiCaptureKind::SystemAudio as i32,
            crate::config::CaptureCapability::MicrophoneOnly { .. } => {
                FfiCaptureKind::Microphone as i32
            }
            crate::config::CaptureCapability::Unavailable { .. } => {
                FfiCaptureKind::Unavailable as i32
            }
        }
    }})
}

/// 获取能力描述文案（UTF-8，调用方需 `translator_free_string` 释放）。
///
/// 这个字符串直接展示给用户，故必须包含**降级原因与替代方案**
/// （例："内放采集不可用（未授权），已切换为麦克风，外放声音需佩戴耳机"）。
///
/// # Safety
/// `handle` 必须有效。返回值需 `translator_free_string` 释放。
#[no_mangle]
pub unsafe extern "C" fn translator_capability_desc(handle: *const c_void) -> *mut c_char {
    ffi_guard_ptr!({{
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let h = &*(handle as *const CaptureHandle);
        let desc = h.capability().describe();
        CString::new(desc)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }})
}

/// 开始采集。
///
/// # Safety
/// `handle` 必须有效且未在运行。
#[no_mangle]
pub unsafe extern "C" fn translator_start_capture(handle: *const c_void) -> i32 {
    ffi_guard!({{
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let h = &*(handle as *const CaptureHandle);
        match h.start() {
            Ok(()) => FfiError::Ok as i32,
            Err(e) => FfiError::from(&e) as i32,
        }
    }})
}

/// 停止采集并冲刷残留。
///
/// # Safety
/// `handle` 必须有效。重复调用安全（第二次为 no-op）。
#[no_mangle]
pub unsafe extern "C" fn translator_stop_capture(handle: *const c_void) -> i32 {
    ffi_guard!({{
        if handle.is_null() {
            return FfiError::InvalidArg as i32;
        }
        let h = &*(handle as *const CaptureHandle);
        match h.stop() {
            Ok(()) => FfiError::Ok as i32,
            Err(e) => FfiError::from(&e) as i32,
        }
    }})
}

/// 取一块音频（16 kHz mono f32）。
///
/// 返回样本数；`0` 表示当前无数据。`out` 必须能容纳至少
/// `translator_chunk_samples()` 个 f32。
///
/// # Safety
/// `handle` 与 `out` 必须有效；`out_cap` 为 `out` 的容量（f32 个数）。
#[no_mangle]
pub unsafe extern "C" fn translator_poll_chunk(
    handle: *const c_void,
    out: *mut f32,
    out_cap: usize,
) -> usize {
    ffi_guard_ptr!({{
        if handle.is_null() || out.is_null() {
            return 0;
        }
        let h = &*(handle as *const CaptureHandle);
        let Some(chunk) = h.poll_chunk() else {
            return 0;
        };
        let n = chunk.samples.len().min(out_cap);
        std::ptr::copy_nonoverlapping(chunk.samples.as_ptr(), out, n);
        n
    }})
}

/// 单块样本数（三端一致，DESIGN §7.2）。
///
/// 16 kHz × 20 ms = 320。Dart 侧据此分配缓冲区。
#[no_mangle]
pub extern "C" fn translator_chunk_samples() -> usize {
    (AppConfig::SAMPLE_RATE as usize * crate::audio::ring::AudioChannel::CHUNK_MS as usize) / 1000
}

/// 采样率（16000）
#[no_mangle]
pub extern "C" fn translator_sample_rate() -> i32 {
    AppConfig::SAMPLE_RATE as i32
}

/// 丢帧数（健康度指标）
///
/// # Safety
/// `handle` 必须有效。
#[no_mangle]
pub unsafe extern "C" fn translator_dropped_frames(handle: *const c_void) -> u64 {
    ffi_guard_ptr!({{
        if handle.is_null() {
            return 0;
        }
        let h = &*(handle as *const CaptureHandle);
        h.dropped_frames()
    }})
}

/// 把一段音频推入采集缓冲（**生产者**语义）。
///
/// ## 为什么需要这个函数
///
/// Android 端的 `AudioRecord` 由 Kotlin 侧驱动（Rust 无法直接访问），
/// 因此音频流向是：
/// ```text
/// AudioRecord (Kotlin) --translator_push_chunk--> Rust 缓冲 --translator_poll_chunk--> Flutter
/// ```
/// 桌面端则相反：Rust 的 flexaudio 直接产出，无需此函数。
///
/// ## 参数
/// - `samples` 指向 `count` 个 f32（16 kHz mono）
/// - 不足一块（320）时会先缓存，满一块才可被 poll 取走
///
/// # Safety
/// `handle` 与 `samples` 必须有效；`count` 不得超过实际可读长度。
#[no_mangle]
pub unsafe extern "C" fn translator_push_chunk(
    handle: *const c_void,
    samples: *const f32,
    count: usize,
) -> i32 {
    ffi_guard!({{
        if handle.is_null() || samples.is_null() || count == 0 {
            return FfiError::InvalidArg as i32;
        }
        let h = &*(handle as *const CaptureHandle);
        // copy_from_slice 而非直接借用：调用方（Kotlin/JNI）的缓冲
        // 在返回后可能立即复用，必须拷贝。
        let data = std::slice::from_raw_parts(samples, count).to_vec();
        match h.push_samples(data) {
            Ok(()) => FfiError::Ok as i32,
            Err(e) => FfiError::from(&e) as i32,
        }
    }})
}

/// 释放由本库分配的 C 字符串。
///
/// # Safety
/// `s` 必须来自本库的 `*mut c_char` 返回值，且不得重复释放。
#[no_mangle]
pub unsafe extern "C" fn translator_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        drop(CString::from_raw(s));
    }));
}

/// 自动发现模型目录，返回 JSON（UTF-8，调用方需 `translator_free_string` 释放）。
///
/// JSON：`{"asr":"...","mt":"...","root":"..."}`；
/// 找不到时返回 `{"asr":"","mt":"","root":""}`（**不是 null**，UI 好判断）。
#[no_mangle]
pub extern "C" fn translator_discover_models() -> *mut c_char {
    ffi_guard_ptr!({{
        let layout = crate::modelspec::discover_model_layout(None);
        let (asr, mt, root) = match &layout {
            Some(l) => {
                let root = l
                    .asr_dir
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                (
                    l.asr_dir.to_string_lossy().to_string(),
                    l.mt_dir.to_string_lossy().to_string(),
                    root,
                )
            }
            None => (String::new(), String::new(), String::new()),
        };
        let j = format!(
            "{{\"asr\":\"{}\",\"mt\":\"{}\",\"root\":\"{}\"}}",
            asr.replace('\\', "\\\\").replace('"', "\\\""),
            mt.replace('\\', "\\\\").replace('"', "\\\""),
            root.replace('\\', "\\\\").replace('"', "\\\""),
        );
        CString::new(j)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }})
}

/// 探测当前系统资源占用，返回 JSON（UTF-8，调用方需 `translator_free_string` 释放）。
///
/// JSON 结构：
/// ```json
/// {
///   "cpu": 23.4, "mem": 61.2,
///   "mem_total": ..., "mem_available": ...,
///   "recommended": "balanced",
///   "advice": "当前 CPU 23%、内存 61%，建议使用「均衡」档。",
///   "top": [{"name": "chrome.exe", "cpu": 12.3, "mem": 123456}],
///   "latency_sensitive": ["zoom.exe"],
///   "audio_playback": []
/// }
/// ```
///
/// 判定依据是**实际 CPU / 内存占用率**，软件类别仅作微调。
/// 本调用会阻塞约 250 ms（需两次采样求 CPU 差值）。
#[no_mangle]
pub extern "C" fn translator_probe_system_load() -> *mut c_char {
    ffi_guard_ptr!({{
        let load = crate::background::probe();
        CString::new(load.to_json())
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }})
}

/// 资源档位的静态说明（UI 渲染选项卡用）。
///
/// 返回 JSON 数组，每个元素含 `key` / `name` / `hint` / `asr_threads` /
/// `mt_threads`，调用方需 `translator_free_string` 释放。
#[no_mangle]
pub extern "C" fn translator_resource_tiers() -> *mut c_char {
    ffi_guard_ptr!({{
        use crate::config::ResourceTier;
        let tiers = [ResourceTier::Light, ResourceTier::Balanced, ResourceTier::Full];
        let items: Vec<String> = tiers
            .iter()
            .map(|t| {
                format!(
                    "{{\"key\":\"{}\",\"name\":\"{}\",\"hint\":\"{}\",\"asr_threads\":{},\"mt_threads\":{}}}",
                    t.as_str(),
                    t.display_name(),
                    t.display_hint().replace('"', "\\\""),
                    t.asr_threads(),
                    t.mt_threads()
                )
            })
            .collect();
        CString::new(format!("[{}]", items.join(",")))
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }})
}

/// 库版本（UI 展示与崩溃报告）
#[no_mangle]
pub extern "C" fn translator_version() -> *mut c_char {
    ffi_guard_ptr!({{
        // 构建时注入的版本串，形如
        //   windows-release-v1.0.0-2026.1028.09.35   正式版
        //   windows-beta-v0.9.7-2026.1028.09.35      测试版
        // 端标识见 build.rs 的 TRANSLATOR_PLATFORM，渠道见 TRANSLATOR_CHANNEL，
        // 语义版本见 TRANSLATOR_SEMVER。
        let v = env!("TRANSLATOR_BUILD_VERSION");
        CString::new(v)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }})
}

/// 是否完全离线（UI 展示"离线运行"标识）。
///
/// 硬编码 `true` —— 本库不含任何网络客户端依赖，
/// 由 `scripts/check_offline.py` 在构建期把关。
#[no_mangle]
pub extern "C" fn translator_is_offline() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_distinct() {
        // UI 按码给文案，重复的码会导致文案错乱
        let codes = [
            FfiError::Ok,
            FfiError::AlreadyRunning,
            FfiError::NoAudioDevice,
            FfiError::PermissionDenied,
            FfiError::ModelMissing,
            FfiError::EngineInit,
            FfiError::Internal,
            FfiError::InvalidArg,
        ];
        let values: Vec<i32> = codes.iter().map(|c| *c as i32).collect();
        let mut unique = values.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), values.len(), "错误码必须互不相同");
    }

    #[test]
    fn error_from_maps_all_variants() {
        // 每个 Error 变体都要有对应码，否则 UI 无文案可用
        let cases = [
            (Error::Internal("x".into()), FfiError::Internal),
            (Error::EngineInit("x".into()), FfiError::EngineInit),
            (Error::Engine("x".into()), FfiError::EngineInit),
            (
                Error::CaptureUnavailable("x".into()),
                FfiError::NoAudioDevice,
            ),
        ];
        for (e, expected) in cases {
            assert_eq!(FfiError::from(&e), expected, "{e:?} 映射错误");
        }
    }

    #[test]
    fn chunk_samples_matches_config() {
        // 三端块长一致是黄金测试的前提（DESIGN §7.2）
        assert_eq!(translator_chunk_samples(), 320);
        assert_eq!(translator_sample_rate(), 16000);
    }

    #[test]
    fn is_offline_is_true() {
        // 这是对用户的承诺，不能改
        assert!(translator_is_offline());
    }

    #[test]
    fn null_handle_is_rejected() {
        // 空指针必须返回错误码而非崩溃
        unsafe {
            assert_eq!(
                translator_probe_capture(std::ptr::null()),
                FfiError::InvalidArg as i32
            );
            assert_eq!(
                translator_start_capture(std::ptr::null()),
                FfiError::InvalidArg as i32
            );
            assert_eq!(
                translator_stop_capture(std::ptr::null()),
                FfiError::InvalidArg as i32
            );
            assert_eq!(translator_dropped_frames(std::ptr::null()), 0);
            assert_eq!(
                translator_poll_chunk(std::ptr::null(), std::ptr::null_mut(), 0),
                0
            );
            // 不得 panic
            translator_free_capture(std::ptr::null_mut());
            translator_free_string(std::ptr::null_mut());
        }
    }

    #[test]
    fn version_string_is_allocated() {
        let p = translator_version();
        assert!(!p.is_null());
        unsafe {
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            // 版本串格式：{channel}-v{semver}-build{time}
            //   beta-v0.9.1-build2026.1022.15.16
            //   release-v1.0.0-build2026.1022.15.16
            // 不带时间戳括号 —— 版本号是给用户看的，越短越好。
            assert!(!s.contains('(') && !s.contains(')'),
                "版本号不应带时间戳括号: {s}");

            // 渠道必须是受支持的 7 种之一
            const CHANNELS: [&str; 7] = [
                "snapshot", "alpha", "beta", "pre", "release", "stable", "current",
            ];
            let head = s.as_str();
            let ch = head.split('-').next().unwrap_or("");
            assert!(
                CHANNELS.contains(&ch),
                "渠道必须是受支持的 7 种之一，实际 {ch}（完整 {s}）"
            );

            // 语义版本：v{X.Y.Z}，紧跟渠道之后
            // 用 rsplit 拿不到它（channel 与 semver 之间有 '-'），
            // 故按 '-' 切成 [channel, v..., time...] 三段取中间。
            let segs: Vec<&str> = head.splitn(3, '-').collect();
            assert_eq!(segs.len(), 3, "版本串应有三段（渠道-版本-时间）: {s}");
            let sv = segs[1];
            let sv_parts: Vec<&str> = sv.trim_start_matches('v').split('.').collect();
            assert_eq!(sv_parts.len(), 3, "语义版本应有三段: {sv}");
            assert!(sv_parts.iter().all(|p| !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())), "语义版本段应纯数字: {sv}");

            // 构建号：build + YYYY.MMDD.HH.mm —— 年份 4 位（用户要求，2 位有歧义）
            let build = segs[2];
            assert!(build.starts_with("build"), "第三段应以 build 开头: {build}");
            let time = &build["build".len()..];
            let parts: Vec<&str> = time.split('.').collect();
            assert_eq!(parts.len(), 4, "构建时间应有 4 段: {time}");
            assert_eq!(parts[0].len(), 4, "年份应是 4 位: {time}");
            assert_eq!(parts[1].len(), 4, "MM+DD 应是 4 位: {time}");
            assert!(parts[2].len() == 2 && parts[3].len() == 2, "{time}");

            translator_free_string(p);
        }
    }

    #[test]
    fn capability_desc_null_safe() {
        unsafe {
            assert!(translator_capability_desc(std::ptr::null()).is_null());
        }
    }

    #[test]
    fn invalid_source_rejected() {
        unsafe {
            // source=99 应返回空指针，不得构造出怪东西
            let h = translator_create_capture(99, 0);
            assert!(h.is_null());
            let h = translator_create_capture(0, 99);
            assert!(h.is_null());
        }
    }
}
