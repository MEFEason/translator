//! 运行时可调参数（2026-10-25 用户要求：转写中动态调整，无需重启）。
//!
//! UI（设置页 Slider 等）通过 FFI `translator_session_update_live` 写入，
//! 引擎循环每轮读取 —— **不需要重建会话、不需要重启转写**。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// 转写过程中可实时调整的参数。
///
/// ## 生效时机
/// - `hangover_ms` / `offline_refine`：**下一句立即生效**（无任何重建）
/// - `asr_threads`：sherpa 的线程数在 recognizer **创建时定死**，改它必须
///   重建 recognizer。为不丢当前句，热切换**推迟到下一句边界**执行
///   （引擎在 VAD 段结束时检测到变化才重建）。
///
///   ⚠️ 重建 recognizer 会丢跨段左上下文 —— 这正是本项目实测过的大坑
///   （「每段新建会话」文本产出只有 100% 的 29%，见 `AsrSession` 文档）。
///   因此热切换**只在用户主动改线程数时发生**，不是常规路径；
///   且切换后第一句的段首识别质量可能短暂下降，UI 需明确提示。
#[derive(Debug, Default)]
pub struct LiveConfig {
    /// ASR 推理线程数（1-8）
    pub asr_threads: AtomicUsize,
    /// VAD hangover（毫秒）—— 句尾静音多久后断句
    pub hangover_ms: AtomicU64,
    /// 是否启用离线精修（两遍解码第二遍）
    pub offline_refine: AtomicBool,
    /// 配置版本号：任何写入都 +1，引擎据此发现「有变化」
    pub version: AtomicU64,
}

impl LiveConfig {
    /// 按创建会话时的配置初始化
    pub fn from_config(asr_threads: usize, hangover_ms: u64, offline_refine: bool) -> Self {
        Self {
            asr_threads: AtomicUsize::new(asr_threads.max(1)),
            hangover_ms: AtomicU64::new(hangover_ms),
            offline_refine: AtomicBool::new(offline_refine),
            version: AtomicU64::new(0),
        }
    }

    /// 更新参数（FFI 入口）。`None` 表示该项不变。
    /// 返回新版本号。
    pub fn update(
        &self,
        asr_threads: Option<usize>,
        hangover_ms: Option<u64>,
        offline_refine: Option<bool>,
    ) -> u64 {
        if let Some(t) = asr_threads {
            self.asr_threads.store(t.max(1), Ordering::Relaxed);
        }
        if let Some(h) = hangover_ms {
            self.hangover_ms.store(h, Ordering::Relaxed);
        }
        if let Some(r) = offline_refine {
            self.offline_refine.store(r, Ordering::Relaxed);
        }
        self.version.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn threads(&self) -> usize {
        self.asr_threads.load(Ordering::Relaxed)
    }
    pub fn hangover(&self) -> u64 {
        self.hangover_ms.load(Ordering::Relaxed)
    }
    pub fn refine_enabled(&self) -> bool {
        self.offline_refine.load(Ordering::Relaxed)
    }
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_bumps_version_and_clamps() {
        let c = LiveConfig::from_config(2, 600, true);
        assert_eq!(c.version(), 0);
        let v = c.update(Some(0), Some(450), Some(false)); // 0 会被夹到 1
        assert_eq!(v, 1);
        assert_eq!(c.threads(), 1);
        assert_eq!(c.hangover(), 450);
        assert!(!c.refine_enabled());
        // None = 不动该项
        c.update(None, None, Some(true));
        assert_eq!(c.threads(), 1);
        assert_eq!(c.hangover(), 450);
        assert!(c.refine_enabled());
        assert_eq!(c.version(), 2);
    }
}
