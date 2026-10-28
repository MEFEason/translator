//! Android 音频采集（AudioPlaybackCapture + 麦克风降级）。
//!
//! ## 两条路径
//!
//! | 路径 | 能力 | 限制 |
//! |---|---|---|
//! | `AudioPlaybackCapture` | 抓其他 App 的播放声音（系统内放） | 需 MediaProjection 授权；**DRM 内容必然静音**；播放方可 opt-out |
//! | `AudioRecord`（麦克风） | 抓本机麦克风 | 需 RECORD_AUDIO；采集不到系统内放 |
//!
//! 卓易通容器内第一条大概率不可用（见 DESIGN §2.5.2），故必须能降级到第二条。
//!
//! ## 强制约束（Android 特有）
//!
//! - **前台服务**：Android 8+ 后台启动受限，Android 14+ 还需
//!   `foregroundServiceType="mediaPlayback"` + `FOREGROUND_SERVICE_MEDIA_PROJECTION`
//! - **MediaProjection 授权不可持久化**：每次冷启动都要重新弹窗（OS 设计）
//! - **卓易通容器**：Manifest 敏感权限会被审计，不达标直接切断系统 API
//!
//! ## 平台代码位置
//!
//! 本文件是**契约与配置**，实际 `AudioRecord` / `MediaProjection` 调用在
//! Flutter 侧的 Kotlin 插件里（`ui/android/.../AudioCapturePlugin.kt`）。
//! Rust 侧只负责：能力探测、参数下发、块长归一化、状态上报。

use crate::audio::PcmChunk;
use crate::config::{AndroidRuntime, CaptureCapability, CaptureSource};
use crate::platform::PermState;
use crate::error::{Error, Result};
use crate::platform::{PermissionGate, SystemAudioCapture};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

/// 统一块长。三端必须一致（§7.2）
const CHUNK_MS: u64 = crate::audio::ring::AudioChannel::CHUNK_MS;
const CHUNK_SAMPLES: usize = (crate::config::AppConfig::SAMPLE_RATE as u64 * CHUNK_MS / 1000) as usize;

/// Android 采集统计
#[derive(Debug, Default)]
pub struct AndroidCaptureStats {
    pub chunks_sent: AtomicU32,
    pub dropped_frames: AtomicU32,
    pub glitches: AtomicU32,
    pub reconnects: AtomicU32,
}

/// 采集能力（Android 侧细化）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AndroidCapability {
    /// 内放可用（MediaProjection 已授权且容器允许）
    PlaybackCapture,
    /// 只能麦克风
    MicrophoneOnly {
        reason: String,
    },
    /// 不可用
    Unavailable {
        reason: String,
    },
}

impl AndroidCapability {
    pub fn to_generic(&self) -> CaptureCapability {
        match self {
            AndroidCapability::PlaybackCapture => CaptureCapability::SystemAudio,
            AndroidCapability::MicrophoneOnly { reason } => CaptureCapability::MicrophoneOnly {
                reason: reason.clone(),
            },
            AndroidCapability::Unavailable { reason } => CaptureCapability::Unavailable {
                reason: reason.clone(),
            },
        }
    }
}

/// 采集请求参数（下发给 Kotlin 插件）
#[derive(Debug, Clone)]
pub struct AndroidCaptureRequest {
    pub source: CaptureSource,
    pub sample_rate: u32,
    pub channel_count: u16,
    /// 是否回声消除（麦克风降级路径用）
    pub enable_aec: bool,
    /// 是否降噪
    pub enable_noise_suppression: bool,
    /// 卓易通容器模式（影响存储与保活策略）
    pub runtime: AndroidRuntime,
}

impl Default for AndroidCaptureRequest {
    fn default() -> Self {
        Self {
            source: CaptureSource::SystemAudio,
            sample_rate: crate::config::AppConfig::SAMPLE_RATE,
            // 请求 stereo 再下混，让系统做混音；也可直接要 mono
            channel_count: 1,
            enable_aec: true,
            enable_noise_suppression: true,
            runtime: AndroidRuntime::Native,
        }
    }
}

/// Android 采集器
pub struct AndroidCapture {
    source: CaptureSource,
    runtime: AndroidRuntime,
    running: Arc<AtomicBool>,
    stats: Arc<AndroidCaptureStats>,
    stop_tx: Option<Sender<()>>,
    /// Kotlin 插件写入的原始音频块（任意长度）
    raw_tx: Option<Sender<PcmChunk>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// 实际生效的采集路径
    active: Option<AndroidCapability>,
}

impl AndroidCapture {
    pub fn new(source: CaptureSource, runtime: AndroidRuntime) -> Self {
        Self {
            source,
            runtime,
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(AndroidCaptureStats::default()),
            stop_tx: None,
            raw_tx: None,
            worker: None,
            active: None,
        }
    }

    pub fn stats(&self) -> Arc<AndroidCaptureStats> {
        Arc::clone(&self.stats)
    }

    /// Kotlin 插件把原始音频块写入此通道。
    ///
    /// 块长任意（Android 侧常见 10~20 ms），由归一化线程切成统一 20 ms。
    pub fn push_raw(&self, samples: Vec<f32>, capture_time_ms: u64) -> bool {
        match &self.raw_tx {
            Some(tx) => tx.send(PcmChunk::new(samples, capture_time_ms)).is_ok(),
            None => false,
        }
    }

    /// 归一化后的块长（三端一致，§7.2）
    pub fn normalized_chunk_samples() -> usize {
        CHUNK_SAMPLES
    }

    /// 探测采集能力。**必须在启动时调用**，结果用于 UI 明示。
    ///
    /// 探测顺序：
    /// 1. 是否在卓易通容器内 → 容器内直接判定 PlaybackCapture 不可用
    /// 2. 系统版本是否 ≥ API 29
    /// 3. 是否已持有 MediaProjection 授权
    pub fn probe(&self, has_projection: bool) -> AndroidCapability {
        if self.runtime.is_container() {
            // 容器内 MediaProjection 需宿主代理，大概率不可用
            // 但仍尝试 —— 不能在探测阶段就否定，留给运行时自检
            return if has_projection {
                AndroidCapability::MicrophoneOnly {
                    reason: "卓易通容器内系统音频捕获能力受限".into(),
                }
            } else {
                AndroidCapability::MicrophoneOnly {
                    reason: "卓易通容器内需宿主代理投屏授权".into(),
                }
            };
        }

        if !has_projection {
            return AndroidCapability::MicrophoneOnly {
                reason: "未授予屏幕录制（MediaProjection）权限".into(),
            };
        }

        if self.source == CaptureSource::Microphone {
            AndroidCapability::MicrophoneOnly {
                reason: "用户选择麦克风模式".into(),
            }
        } else {
            AndroidCapability::PlaybackCapture
        }
    }

}

impl SystemAudioCapture for AndroidCapture {
    fn start(&mut self, mut on_chunk: Box<dyn FnMut(PcmChunk) + Send>) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Err(Error::Internal("采集已在运行".into()));
        }

        // 请求参数下发给 Kotlin 插件（通过 FFI / MethodChannel）
        let request = AndroidCaptureRequest {
            source: self.source,
            runtime: self.runtime,
            ..Default::default()
        };

        // 卓易通容器内必须以前台服务运行，否则容器休眠会杀掉采集
        if self.runtime.is_container() && self.source == CaptureSource::SystemAudio {
            // 前台服务由 Kotlin 侧保证；Rust 侧记录要求
            tracing_warn("卓易通容器内系统音频采集需前台服务，请确认已授权");
        }

        self.running.store(true, Ordering::Relaxed);
        self.active = Some(self.probe(/* has_projection = */ false));

        let (stop_tx, stop_rx) = channel::<()>();
        // raw 来自 Kotlin 插件（任意块长），out 归一化后交给上层
        let (raw_tx, raw_rx) = channel::<PcmChunk>();
        let running = Arc::clone(&self.running);
        let stats = Arc::clone(&self.stats);

        // 归一化线程：把 Kotlin 的任意块长切成统一 20 ms
        let worker = std::thread::Builder::new()
            .name("android-normalize".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let mut pending: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES * 8);
                while running.load(Ordering::Relaxed) {
                    match raw_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                        Ok(chunk) => {
                            pending.extend_from_slice(&chunk.samples);
                            let elapsed = t0.elapsed().as_millis() as u64;
                            let mut offset = 0usize;
                            while pending.len() - offset >= CHUNK_SAMPLES {
                                let mut s: Vec<f32> =
                                    pending[offset..offset + CHUNK_SAMPLES].to_vec();
                                crate::audio::resample::normalize_peak(&mut s, -1.0);
                                // 送回上层回调（经由 raw_tx 反向不可行，改用 on_chunk）
                                on_chunk(PcmChunk::new(s, elapsed));
                                offset += CHUNK_SAMPLES;
                                stats.chunks_sent.fetch_add(1, Ordering::Relaxed);
                            }
                            if offset > 0 {
                                pending.drain(0..offset);
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if stop_rx.try_recv().is_ok() {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let _ = raw_tx;
                let _ = request;
            })
            .map_err(|e| Error::Internal(format!("采集线程创建失败: {e}")))?;

        self.stop_tx = Some(stop_tx);
        self.raw_tx = Some(raw_tx);
        self.worker = Some(worker);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        self.raw_tx = None;
        Ok(())
    }

    fn capability(&self) -> CaptureCapability {
        self.active
            .as_ref()
            .map(|c| c.to_generic())
            .unwrap_or_else(|| CaptureCapability::Unavailable {
                reason: "采集未启动".into(),
            })
    }

    fn source_label(&self) -> String {
        match (self.source, self.runtime) {
            (CaptureSource::SystemAudio, AndroidRuntime::Native) => {
                "系统内放（AudioPlaybackCapture）".into()
            }
            (CaptureSource::SystemAudio, AndroidRuntime::ZhuoYiTong) => {
                "卓易通容器 · 降级为麦克风".into()
            }
            (CaptureSource::Microphone, _) => "麦克风（AEC + 降噪）".into(),
        }
    }

    fn dropped_frames(&self) -> u64 {
        self.stats.dropped_frames.load(Ordering::Relaxed) as u64
    }
}

impl Drop for AndroidCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// 外部注入采集器（Android 会话链路的**唯一可用实现**）。
///
/// ## 为什么需要它（2026-10-26）
///
/// 上面的 [`AndroidCapture`] 是 push 模型契约，但 `platform::create_capture`
/// 从不构造它（Android 上直接返回 `CaptureUnavailable`）→ 会话层
/// `CaptureHandle::create` 失败 → `session_start` 因 `sources.is_empty()`
/// 直接 InvalidArg —— **Android 会话根本起不来**。
///
/// 本实现补上这条构造路径：真正的生产者（AudioRecord /
/// AudioPlaybackCapture）在 Kotlin 侧，音频经 FFI
/// `translator_push_chunk`（采集级）或 `translator_session_push_chunk`
/// （会话级便捷入口）灌进 `CaptureHandle.chunk_tx`，本对象只维持
/// 「可启动 + 如实上报能力」的最小契约。
///
/// ## 生命周期（与 CaptureHandle 的约定）
///
/// - `start()`：no-op 返回 Ok。running 标志由 `CaptureHandle::start`
///   在调用本方法**之后**置位（ffi.rs L165），`push_samples` 的 running
///   校验由此满足 —— 推送窗口 = 会话运行期，正确。
/// - `stop()`：no-op 返回 Ok。Kotlin 侧由 MethodChannel 单独停止；
///   `CaptureHandle::stop` 会先把 running 清零，之后到达的推送被拒。
/// - on_chunk 回调**不使用**：音频不经过本对象（推送直接进队列），
///   回调随参数丢弃，合法。
pub struct ExternalPushCapture {
    source: CaptureSource,
    runtime: AndroidRuntime,
    /// 内放授权标志：0=未授权 1=MediaProjection 已授予。
    /// Kotlin 侧拿到授权后经 FFI `translator_session_set_capture_granted`
    /// 更新，capability 随之反映真实能力（禁止静默误报）。
    granted: AtomicU32,
}

impl ExternalPushCapture {
    pub fn new(source: CaptureSource, runtime: AndroidRuntime) -> Self {
        Self { source, runtime, granted: AtomicU32::new(0) }
    }

    /// 由会话层调用：内放（MediaProjection）授权状态变更。
    pub fn set_granted(&self, granted: bool) {
        self.granted.store(granted as u32, Ordering::Relaxed);
    }

    pub fn is_granted(&self) -> bool {
        self.granted.load(Ordering::Relaxed) != 0
    }
}

impl SystemAudioCapture for ExternalPushCapture {
    fn start(&mut self, _on_chunk: Box<dyn FnMut(PcmChunk) + Send>) -> Result<()> {
        // 生产者在外（Kotlin），本对象无内部线程可起。
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn capability(&self) -> CaptureCapability {
        // 麦克风源：注入链路对 AudioRecord 完全可用（与桌面麦克风同义，
        // 不触发「降级」横幅）。
        // 内放源：依赖 MediaProjection 授权，授权状态由 Kotlin 回传。
        match (self.source, self.runtime) {
            (CaptureSource::Microphone, _) => CaptureCapability::SystemAudio,
            (CaptureSource::SystemAudio, AndroidRuntime::ZhuoYiTong) => {
                CaptureCapability::MicrophoneOnly {
                    reason: "卓易通容器内系统音频捕获不可用，已降级为麦克风".into(),
                }
            }
            (CaptureSource::SystemAudio, AndroidRuntime::Native) => {
                if self.is_granted() {
                    CaptureCapability::SystemAudio
                } else {
                    CaptureCapability::MicrophoneOnly {
                        reason: "未授予屏幕录制（MediaProjection）权限".into(),
                    }
                }
            }
        }
    }

    fn source_label(&self) -> String {
        match (self.source, self.runtime) {
            (CaptureSource::SystemAudio, AndroidRuntime::Native) => {
                "系统内放（AudioPlaybackCapture · Kotlin 注入）".into()
            }
            (CaptureSource::SystemAudio, AndroidRuntime::ZhuoYiTong) => {
                "卓易通容器 · 降级为麦克风（Kotlin 注入）".into()
            }
            (CaptureSource::Microphone, _) => "麦克风（Kotlin AudioRecord 注入）".into(),
        }
    }

    fn dropped_frames(&self) -> u64 {
        0
    }

    /// Kotlin 回传 MediaProjection 授权状态 → 能力探测随之变化。
    fn set_capture_granted(&self, granted: bool) {
        self.set_granted(granted);
    }
}

/// 无宏依赖的告警（避免为一行日志引入依赖）。
///
/// ⚠️ 2026-10-26 修复：旧实现在 `capture-android` feature 下引用
/// 不存在的 `android_log` crate —— 该 feature 一开就编译失败，
/// 也从没被任何构造路径用上所以从未暴露。
///
/// ⚠️ 2026-10-27 再修复：**Android 上 `eprintln!` 完全不进 logcat**
/// （实测 0 行）。旧注释说「stderr 由 zygote 重定向进 logcat（tag=libc）」
/// 是**错的** —— 那是 zygote 自己的 stderr，应用进程的 stderr 会被
/// zygote 丢弃。结果：Rust 侧的任何诊断信息（含模型加载失败原因）
/// 在真机日志里**完全看不到**，只能靠猜。
///
/// 改用 `__android_log_write`（libc 自带的系统日志接口，不引入任何依赖）：
/// 真机可用 `adb logcat -s translator_core:V` 直接观察。
fn tracing_warn(msg: &str) {
    android_log("W", msg);
}

/// 写一条 Android 系统日志（tag = `translator_core`）。
///
/// 走 `__android_log_write`（NDK 的 liblog 里导出，无需 crate 依赖）。
/// 非 Android 平台退化为 `eprintln!`（桌面端 stderr 可见）。
pub fn android_log(level: &str, msg: &str) {
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        // ANDROID_LOG_INFO = 4, ANDROID_LOG_WARN = 5, ANDROID_LOG_ERROR = 6
        let prio = match level {
            "E" => 6,
            "W" => 5,
            _ => 4,
        };
        let tag = c"translator_core";
        let text = match CString::new(msg) {
            Ok(t) => t,
            Err(_) => return, // 内含 NUL → 丢弃
        };
        // __android_log_write(prio, tag, text)
        unsafe extern "C" {
            fn __android_log_write(
                prio: std::os::raw::c_int,
                tag: *const std::os::raw::c_char,
                text: *const std::os::raw::c_char,
            ) -> std::os::raw::c_int;
        }
        unsafe {
            __android_log_write(prio, tag.as_ptr(), text.as_ptr());
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!("[translator_core][{level}] {msg}");
    }
}

/// 便捷宏：Android 上打 INFO 级日志（桌面端退化为 eprintln!）。
#[macro_export]
macro_rules! alog {
    ($($arg:tt)*) => {
        $crate::platform::android::android_log("I", &format!($($arg)*))
    };
}

/// Android 权限门控
pub struct AndroidPermission;

impl PermissionGate for AndroidPermission {
    fn status(&self) -> PermState {
        // 实际状态由 Kotlin 侧查询后回传
        PermState::Denied
    }
    fn request(&self) -> PermState {
        // 必须由 Flutter 层调起 MediaProjection Intent
        PermState::NeedsManualGrant
    }
    fn open_settings(&self) {
        // Android: ACTION_APPLICATION_DETAILS_SETTINGS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_matches_global_config() {
        assert_eq!(CHUNK_MS, 20);
        assert_eq!(CHUNK_SAMPLES, 320);
    }

    #[test]
    fn container_never_reports_playback_capture() {
        // 关键不变量：卓易通容器内不得上报内放可用
        let c = AndroidCapture::new(
            CaptureSource::SystemAudio,
            AndroidRuntime::ZhuoYiTong,
        );
        for has_proj in [true, false] {
            let cap = c.probe(has_proj);
            assert!(
                !matches!(cap, AndroidCapability::PlaybackCapture),
                "容器内不得上报 PlaybackCapture（has_projection={has_proj}）"
            );
        }
    }

    #[test]
    fn native_without_projection_falls_back_to_mic() {
        let c = AndroidCapture::new(CaptureSource::SystemAudio, AndroidRuntime::Native);
        let cap = c.probe(false);
        assert!(matches!(cap, AndroidCapability::MicrophoneOnly { .. }));
        assert!(cap.to_generic().is_degraded(), "必须标记为降级");
    }

    #[test]
    fn native_with_projection_supports_playback() {
        let c = AndroidCapture::new(CaptureSource::SystemAudio, AndroidRuntime::Native);
        let cap = c.probe(true);
        assert!(matches!(cap, AndroidCapability::PlaybackCapture));
        assert_eq!(cap.to_generic(), CaptureCapability::SystemAudio);
    }

    #[test]
    fn degraded_capability_message_is_actionable() {
        let c = AndroidCapture::new(CaptureSource::SystemAudio, AndroidRuntime::ZhuoYiTong);
        let d = c.probe(false).to_generic().describe();
        assert!(d.contains("麦克风"), "必须提示替代方案: {d}");
    }

    #[test]
    fn request_defaults_match_config() {
        let r = AndroidCaptureRequest::default();
        assert_eq!(r.sample_rate, crate::config::AppConfig::SAMPLE_RATE);
        assert!(r.enable_aec);
    }

    #[test]
    fn stop_before_start_is_safe() {
        let mut c = AndroidCapture::new(CaptureSource::SystemAudio, AndroidRuntime::Native);
        let _ = c.stop();
    }

    #[test]
    fn label_reflects_runtime() {
        let native = AndroidCapture::new(CaptureSource::SystemAudio, AndroidRuntime::Native);
        assert!(native.source_label().contains("AudioPlaybackCapture"));

        let container = AndroidCapture::new(CaptureSource::SystemAudio, AndroidRuntime::ZhuoYiTong);
        assert!(container.source_label().contains("卓易通"));
        assert!(container.source_label().contains("降级"));
    }
}
