//! Translator 核心库。
//!
//! 架构约束（详见 docs/DESIGN.md §7.1）：
//! - 本 crate 内**不允许出现任何 `#[cfg(target_os)]`**
//! - 平台差异全部收敛到 `platform/` 下的 4 个 trait 实现
//! - DSP / ASR / MT / 词库 / 字幕聚合逻辑三端唯一实现

pub mod audio;
pub mod asr;
pub mod background;
pub mod config;
pub mod error;
// ffi.rs 里的 ffi_guard! / ffi_guard_ptr! 宏要被 ffi_session.rs 使用，
// 故用 #[macro_use] 让它们在后续模块中可见。
// （宏体已改成完全限定路径，避免在别的模块里出现 hygiene 问题。）
#[macro_use]
pub mod ffi;
pub mod ffi_session;
pub mod glossary;
pub mod modelspec;
pub mod mt;
pub mod offline;
pub mod pipeline;
pub mod platform;
pub mod subtitle;
pub mod text_normalize;
pub mod transcript;
pub mod vad;

pub use config::AppConfig;
pub use error::{Error, Result};
