//! 空翻译后端（MT 可选模式）。
//!
//! ## 为什么需要它
//!
//! 阶段1（Android 先跑 ASR）设备上**只装了 ASR 模型**，没有 Bergamot 权重。
//! 而 `resolve_backends` 里 ASR 与 MT 是串联的 `?`：
//! MT 建不起来 → 整个引擎加载失败 → 连识别都不工作。
//!
//! 这个后端让「翻译不可用」变成一个**可表达的状态**，而不是致命错误：
//! - 引擎正常启动，识别照常出原文；
//! - 译文恒为空（UI 只显示原文行）；
//! - `is_available()` 返回 false → UI 可显式提示「翻译引擎未启用」，
//!   **禁止静默降级**（DESIGN §2.5.2）。
//!
//! ## 与 Mock 的区别
//!
//! [`super::mock`] 是**假翻译**（英文→固定中文），用于联调与测试；
//! 本后端是**无翻译**（恒空），用于「确实没有翻译引擎」的真实场景。
//! 两者都不可用于生产出译文，但语义完全不同，别混用。

use super::{MtBackend, MtConfig, QualityTier, Translation};
use crate::error::Result;

/// 空翻译后端。
///
/// `translate` 恒返回空文本 —— 正常路径不会走到这里
/// （`mt_loop` 会先判 `is_available()` 跳过），
/// 万一被调到也只返回空串，不会崩。
pub struct NullMt {
    /// 不可用原因（供 UI 如实展示）
    reason: String,
}

impl NullMt {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into() }
    }
}

impl MtBackend for NullMt {
    fn translate(&self, _protected_text: &str) -> Result<Translation> {
        Ok(Translation {
            text: String::new(),
            // 没有译文就谈不上哨兵往返一致性 —— 标 false 会让 UI
            // 把它当「需人工复核」的错误提示，故标 true（视为「无需翻译」）。
            consistent: true,
            elapsed_ms: 0,
        })
    }

    fn name(&self) -> &'static str {
        "null"
    }

    fn is_available(&self) -> bool {
        false
    }

    fn unavailable_reason(&self) -> Option<String> {
        Some(self.reason.clone())
    }

    fn tier(&self) -> QualityTier {
        QualityTier::Fast
    }
}

/// 便捷构造：给「MT 后端构建失败」的场景用。
pub fn null_for(reason: impl Into<String>) -> super::SharedMt {
    std::sync::Arc::new(NullMt::new(reason))
}

/// 保留 `MtConfig` 引用，避免未使用告警（配置在可选模式下仍会被构造）。
#[allow(dead_code)]
fn _unused(_c: &MtConfig) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_unavailable_with_reason() {
        let m = NullMt::new("未安装 Bergamot 权重");
        assert!(!m.is_available());
        assert_eq!(m.unavailable_reason().as_deref(), Some("未安装 Bergamot 权重"));
    }

    #[test]
    fn translate_returns_empty_without_panic() {
        let m = NullMt::new("x");
        let t = m.translate("hello").unwrap();
        assert!(t.text.is_empty());
        assert!(t.consistent);
    }
}
