//! 平台抽象：全部平台差异收敛于此。
//!
//! 架构硬约束（DESIGN.md §7.1）：
//! **本 crate 内不允许出现任何 `#[cfg(target_os)]`**。
//! 平台特定代码只允许出现在实现这 4 个 trait 的模块里。
//! 若发现自己在业务逻辑里写平台分支，说明抽象漏了。

use crate::audio::PcmChunk;
use crate::config::{AppConfig, CaptureCapability};
use crate::subtitle::SubtitleView;

/// Android 采集层。契约与配置在 Rust 侧，实际 AudioRecord 调用在 Kotlin 插件。
pub mod android;

/// 本地时间（转写盘文件名用）。
pub mod clock;

/// 用户可见目录解析（转写盘默认落点）。
///
/// 单独于 `desktop.rs`：**不依赖 `capture-desktop` feature**，
/// 因为「桌面在哪」这个问题与「能不能采集系统音频」无关。
pub mod userdir;

/// Windows / macOS 采集层：依赖 flexaudio（成熟实现，见 DESIGN §2.1 修订）
#[cfg(feature = "capture-desktop")]
pub mod desktop;

/// 创建平台对应的采集器。
///
/// 这是 FFI 边界唯一入口 —— 让 `ffi.rs` 无需知道平台细节，
/// 也保证业务逻辑里不出现 `#[cfg(target_os)]`（DESIGN §7.1）。
pub fn create_capture(
    source: crate::config::CaptureSource,
    runtime: crate::config::AndroidRuntime,
) -> crate::Result<Box<dyn SystemAudioCapture>> {
    // 未启用任何采集 feature 时两个参数都可能不被使用（消警告）
    let _ = (&source, &runtime);
    // 桌面端：Windows + macOS 都由 flexaudio 统一处理
    #[cfg(feature = "capture-desktop")]
    {
        if !matches!(runtime, crate::config::AndroidRuntime::ZhuoYiTong) {
            return Ok(Box::new(desktop::DesktopCapture::new(source)));
        }
    }

    // Android 原生 + 卓易通容器：契约在 Rust，实现在 Kotlin 插件。
    //
    // capture-android feature 开启时返回**外部注入采集器**：
    // 音频生产者（AudioRecord / AudioPlaybackCapture）在 Kotlin 侧，
    // 经 translator_push_chunk / translator_session_push_chunk 灌入
    // CaptureHandle 队列。这是 Android 会话能启动的前提
    // （2026-10-26 修复：旧实现一律 CaptureUnavailable → 会话起不来）。
    #[cfg(feature = "capture-android")]
    {
        if !matches!(runtime, crate::config::AndroidRuntime::ZhuoYiTong)
            || matches!(source, crate::config::CaptureSource::Microphone)
        {
            return Ok(Box::new(android::ExternalPushCapture::new(source, runtime)));
        }
        // 容器 + 内放源 = 不可用（卓易通无 AudioPlaybackCapture），
        // 让调用方（会话）按麦克风兜底源继续 —— 双源场景麦克风那路
        // 仍会成功创建，UI 经 capture_capability 如实展示降级。
    }

    // 其他平台（或未启用采集 feature）：如实上报不可用
    Err(crate::Error::CaptureUnavailable(format!(
        "当前平台未启用音频采集支持（runtime={runtime:?}）"
    )))
}

/// 采集控制句柄

/// 采集统计快照（跨平台的最小公共信息）。
///
/// 诊断用：判断「麦克风到底采没采到声音」。
/// `rms` 恒为 0 → 设备/权限/静音问题；明显 > 0 → 音频进来了，
/// 问题在下游（VAD 切分 / ASR 识别）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStatsSnapshot {
    /// 最近一块音频的 RMS（0..1）
    pub rms: f32,
    /// 采集期间出现过的最大 RMS（0..1）—— 用来定 VAD 阈值
    pub max_rms: f32,
    /// 采集源的原生采样率（Hz）
    pub native_rate: u32,
    /// 采集源的原生声道数
    pub native_channels: u32,
    /// 采集到的总块数
    pub chunks: u64,
    /// 上游环形缓冲丢掉的帧数
    pub dropped: u64,
}

pub trait SystemAudioCapture {
    /// 启动采集。音频块经回调上送。
    ///
    /// 回调在音频实时线程上被调用，**绝不能阻塞**（DESIGN.md §6.2）。
    fn start(&mut self, on_chunk: Box<dyn FnMut(PcmChunk) + Send>) -> crate::Result<()>;

    /// 停止采集
    fn stop(&mut self) -> crate::Result<()>;

    /// 当前能力。启动后应保持稳定。
    fn capability(&self) -> CaptureCapability;

    /// 人类可读的采集源描述（用于 UI）
    fn source_label(&self) -> String;

    /// 采集已丢失音频的帧数（健康度）
    fn dropped_frames(&self) -> u64;

    /// 外部注入授权变更（Android 专用语义）。
    ///
    /// Kotlin 侧拿到 MediaProjection 授权后回传，采集器能力随之更新。
    /// 桌面实现是内部自采、无外部授权概念 → 默认 no-op。
    /// 收敛在 trait 里，避免业务层出现平台分支（§7.1）。
    fn set_capture_granted(&self, _granted: bool) {}



    /// 诊断用：采集音频环形缓冲。默认 None —— 移动端实现可不提供。

    fn capture_stats_snapshot(&self) -> CaptureStatsSnapshot {
        CaptureStatsSnapshot::default()
    }
}

/// 权限状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermState {
    Granted,
    Denied,
    /// 需要用户手动到系统设置开启（macOS TCC / 卓易通应用管理）
    NeedsManualGrant,
    /// 平台不支持（Windows 内放无需权限）
    NotRequired,
}

impl PermState {
    pub fn is_ok(&self) -> bool {
        matches!(self, PermState::Granted | PermState::NotRequired)
    }
}

/// 权限门控
pub trait PermissionGate {
    fn status(&self) -> PermState;
    fn request(&self) -> PermState;
    /// 打开系统设置页。macOS 与卓易通需要用户手动勾选。
    fn open_settings(&self);
}

/// 悬浮字幕窗
pub trait OverlayWindow {
    fn show(&self, view: &SubtitleView);
    fn hide(&self);
    fn set_always_on_top(&mut self, on: bool);
    /// 桌面端支持透明背景，移动端忽略
    fn set_opacity(&mut self, opacity: f32);
}

/// 配置持久化
pub trait ConfigStore {
    fn load(&self) -> AppConfig;
    fn save(&self, cfg: &AppConfig) -> crate::Result<()>;
}

/// 平台能力总描述。用于启动自检与 UI 展示。
#[derive(Debug, Clone)]
pub struct PlatformInfo {
    pub os_name: String,
    pub os_version: String,
    pub android_runtime: Option<crate::config::AndroidRuntime>,
    pub capture: CaptureCapability,
    pub capture_label: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_state_ok_semantics() {
        assert!(PermState::Granted.is_ok());
        assert!(PermState::NotRequired.is_ok());
        assert!(!PermState::Denied.is_ok());
        assert!(!PermState::NeedsManualGrant.is_ok());
    }

    #[test]
    fn capability_describe_mentions_degradation() {
        let c = CaptureCapability::MicrophoneOnly {
            reason: "容器内不支持".to_string(),
        };
        assert!(c.is_usable());
        assert!(c.is_degraded());
        let d = c.describe();
        assert!(d.contains("不可用"));
        assert!(d.contains("麦克风"), "降级提示必须说明替代方案");
    }

    #[test]
    fn system_audio_not_degraded() {
        let c = CaptureCapability::SystemAudio;
        assert!(c.is_usable());
        assert!(!c.is_degraded());
        assert_eq!(c.describe(), "内放采集：可用");
    }

    #[test]
    fn unavailable_is_not_usable() {
        let c = CaptureCapability::Unavailable {
            reason: "无音频设备".to_string(),
        };
        assert!(!c.is_usable());
        assert!(!c.is_degraded());
    }
}
