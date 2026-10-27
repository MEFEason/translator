//! 后端装配：**整个 crate 里唯一允许出现 `#[cfg(feature)]` 的文件**。
//!
//! ## 为什么单独隔离
//!
//! `asr-sherpa` / `mt-bergamot` 会引入原生库依赖，而编排层只该依赖
//! `AsrBackend` / `MtBackend` trait。若 `#[cfg]` 散落在 engine 里，
//! 「某个平台/某个构建下用了哪个引擎」这件事就没人能一眼看全，
//! 而这恰恰是最容易出错、最难排查的一环。
//!
//! 收敛到本文件后：engine 永远只看见 trait，wiring 永远只有几行 cfg，
//! 出问题只需查这一个文件。

use std::sync::Arc;

use crate::asr::{AsrBackend, AsrConfig};
use crate::error::{Error, Result};
use crate::mt::{MtConfig, SharedMt};

/// 后端来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// 按编译 feature 选真实引擎；未编译该 feature 时报错
    Auto,
    /// 强制用内置参考实现。
    ///
    /// 不是「测试专用开关」——`asr/mock.rs` 本身就无条件编进 cdylib，
    /// 用途是原生库就绪前先跑通链路。它产出 `word0 word1 …`，
    /// 所以看到这类输出是正常的，看不到字幕才是 bug。
    Mock,
}

/// 构建 ASR 后端。
pub fn build_asr_backend(cfg: &AsrConfig, choice: BackendChoice) -> Result<Arc<dyn AsrBackend>> {
    match choice {
        BackendChoice::Mock => Ok(Arc::new(crate::asr::mock::MockBackend::new())),
        BackendChoice::Auto => real_asr(cfg),
    }
}

/// 构建 MT 后端。
///
/// ## MT 可选（2026-10-26，Android 阶段1）
///
/// 设备上可能**只装了 ASR 模型**（MT 权重 50MB，阶段2 才接）。
/// 此时 MT 建不起来 —— 但这**不该让整个引擎加载失败**：
/// 识别是主价值，翻译是增强。
///
/// 故 MT 失败时降级为 [`crate::mt::null::NullMt`]：
/// 引擎正常启动，识别照常出原文，译文恒空，
/// `is_available() == false` 让 UI 能显式提示「翻译引擎未启用」
/// （DESIGN §2.5.2 禁止静默降级）。
///
/// ASR 失败**仍然致命** —— 没有识别就没有任何输出。
pub fn build_mt_backend(cfg: &MtConfig, choice: BackendChoice) -> Result<SharedMt> {
    match choice {
        BackendChoice::Mock => {
            Ok(Arc::new(crate::mt::mock::MockTranslator::new(cfg.tier)) as SharedMt)
        }
        BackendChoice::Auto => match real_mt(cfg) {
            Ok(m) => Ok(m),
            Err(e) => {
                eprintln!("[wiring] MT 后端不可用，降级为「仅识别」模式: {e}");
                Ok(crate::mt::null::null_for(format!(
                    "翻译引擎未启用（{e}）"
                )))
            }
        },
    }
}

#[cfg(any(feature = "asr-sherpa", feature = "asr-sherpa-android"))]
fn real_asr(cfg: &AsrConfig) -> Result<Arc<dyn AsrBackend>> {
    Ok(Arc::new(crate::asr::sherpa::SherpaBackend::load(cfg)?))
}

#[cfg(not(any(feature = "asr-sherpa", feature = "asr-sherpa-android")))]
fn real_asr(_cfg: &AsrConfig) -> Result<Arc<dyn AsrBackend>> {
    Err(Error::ModelMissing(
        "当前构建未启用 asr-sherpa feature，无法加载真实 ASR 引擎。\n\
         构建时加上：--features asr-sherpa\n\
         联调可先用 BackendChoice::Mock。"
            .into(),
    ))
}

#[cfg(feature = "mt-bergamot")]
fn real_mt(cfg: &MtConfig) -> Result<SharedMt> {
    Ok(Arc::new(crate::mt::bergamot::BergamotTranslator::load(cfg)?) as SharedMt)
}

#[cfg(not(feature = "mt-bergamot"))]
fn real_mt(_cfg: &MtConfig) -> Result<SharedMt> {
    Err(Error::ModelMissing(
        "当前构建未启用 mt-bergamot feature，无法加载真实翻译引擎。\n\
         构建时加上：--features mt-bergamot\n\
         联调可先用 BackendChoice::Mock。"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_choice_needs_no_model_dir() {
        // 联调模式必须能在「完全没有模型文件」时跑通，
        // 否则自检工具在拿到权重之前就废了
        let cfg = AsrConfig::new("no-such-dir", 1);
        assert!(build_asr_backend(&cfg, BackendChoice::Mock).is_ok());
    }

    #[test]
    fn auto_choice_without_feature_explains_how_to_enable() {
        // 错误信息必须可操作：只说「失败」等于没说
        let cfg = AsrConfig::new("no-such-dir", 1);
        match build_asr_backend(&cfg, BackendChoice::Auto) {
            Err(Error::ModelMissing(msg)) => {
                #[cfg(not(any(feature = "asr-sherpa", feature = "asr-sherpa-android")))]
                assert!(msg.contains("--features asr-sherpa"), "{msg}");
                #[cfg(any(feature = "asr-sherpa", feature = "asr-sherpa-android"))]
                assert!(msg.contains("no-such-dir"), "{msg}");
            }
            Err(other) => panic!("期望 ModelMissing，得到 {other:?}"),
            // dyn AsrBackend 不实现 Debug，成功路径只能断言不报错
            Ok(_) => {}
        }
    }

    #[test]
    fn mock_mt_respects_tier() {
        let mut cfg = MtConfig::new("no-such-dir", 1);
        cfg.tier = crate::mt::QualityTier::Accurate;
        let mt = build_mt_backend(&cfg, BackendChoice::Mock).unwrap();
        assert_eq!(mt.tier(), crate::mt::QualityTier::Accurate);
    }
}