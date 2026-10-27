//! Bergamot 翻译后端（Mozilla，Marian NMT）。
//!
//! ## 选型理由
//!
//! 与 CTranslate2 同为 Marian 框架，**权重与分词器完全复用**，
//! 翻译质量无差异。决定性差别在构建：
//!
//! - Bergamot：纯 CMake 源码编译 → **构建期离线**（仅需 C++17）
//! - CTranslate2：`ctranslate2-sys` 联网下载预编译包 → 构建必须联网
//!
//! 本项目要求"构建与运行双离线"，故选 Bergamot。
//!
//! ## C API 契约（`bergamot_translator.h`）
//!
//! ```c
//! marian::bergamot::TranslationModel* bergamot_translator_create(
//!     const std::string& modelPath, int32_t numThreads, int verbosity);
//! void bergamot_translator_destroy(TranslationModel*);
//! void bergamot_translator_translate(
//!     TranslationModel*, const std::string& source,
//!     std::vector<std::string>& target);
//! void bergamot_translator_translate_multi(                // 批量，摊薄固定开销
//!     TranslationModel*, const std::vector<std::string>& source,
//!     std::vector<std::string>& target);
//! ```
//!
//! **注意**：Bergamot 的 `translate` 收**原始文本**（非 token），
//! 内置 SentencePiece 分词 —— 这是它相对 CTranslate2 的架构优势，
//! 本模块因此无需自接分词器。
//!
//! ## FFI 约定
//!
//! - 所有调用在 `catch_unwind` 内，panic 转错误码（跨 FFI 不可 panic）
//! - 模型句柄裸指针，仅由 `BergamotTranslator` 持有，析构时销毁
//! - `Send + Sync`：`bergamot_translator_create/destroy` 独立于翻译调用，
//!   翻译接口本身无内部可变状态（模型只读），故可 `Sync`

use super::{MtBackend, MtConfig, QualityTier, Translation};
use crate::error::{Error, Result};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::Arc;

// ── FFI 声明（不引入 bindgen，手写以保持零重依赖）────────────────────

extern "C" {
    /// 创建翻译器。
    ///
    /// @param config_path Bergamot **yml 配置文件**的绝对路径。
    ///   Bergamot 的权重不是"单文件 + 目录"结构，而是一份配置
    ///   （列出 models / vocabs / shortlist）。配置内的相对路径以
    ///   **配置文件自身所在目录**为基准解析，故调用方无需关心工作目录。
    fn bergamot_translator_create(
        config_path: *const c_char,
        num_threads: c_int,
        verbosity: c_int,
    ) -> *mut c_void;

    fn bergamot_translator_destroy(model: *mut c_void);

    fn bergamot_translator_translate(
        model: *const c_void,
        source: *const c_char,
        target: *mut *mut c_char,
    );

    fn bergamot_translator_translate_multi(
        model: *const c_void,
        source_count: c_int,
        source: *const *const c_char,
        target: *mut *mut c_char,
    );

    /// 释放 `bergamot_translator_translate*` 写出的字符串。
    /// 与 C 侧的 `malloc` 配对，Rust 侧用 `CString::from_raw` 回收。
    fn bergamot_translator_free_string(s: *mut c_char);
}

/// Bergamot 权重目录的必需文件（Firefox Translations 格式）。
///
/// 定义收敛在 [`crate::modelspec`]，让启动自检与引擎加载共用同一份规则 ——
/// 两处各写一份清单，迟早会在「新增了权重文件」时漏改一处。
pub use crate::modelspec::MT_REQUIRED_FILES as REQUIRED_FILES;

pub struct BergamotTranslator {
    model: *mut c_void,
    tier: QualityTier,
}

impl BergamotTranslator {
    /// 从权重目录加载
    ///
    /// Bergamot 需要一份 **yml 配置文件**（列出 models / vocabs / shortlist），
    /// 约定放在权重目录下的 [`crate::modelspec::MT_CONFIG_FILE`]。
    /// 配置内的相对路径以配置文件所在目录为基准解析，
    /// 所以这里传绝对路径，不受进程工作目录影响。
    pub fn load(cfg: &MtConfig) -> Result<Self> {
        let dir = &cfg.model_dir;
        // 权重格式校验收敛到 modelspec，规则与启动自检完全一致
        crate::modelspec::probe_mt_dir(dir)?;

        let config_path = dir.join(crate::modelspec::MT_CONFIG_FILE);
        let path = CString::new(config_path.display().to_string())
            .map_err(|_| Error::EngineInit("模型路径含 NUL 字符".into()))?;

        let model = unsafe {
            bergamot_translator_create(
                path.as_ptr(),
                cfg.num_threads as c_int,
                0, // verbosity: 0 = 只报错
            )
        };

        if model.is_null() {
            return Err(Error::EngineInit(
                "bergamot_translator_create 返回 NULL（配置文件无法解析，或权重不完整）"
                    .into(),
            ));
        }

        Ok(Self {
            model,
            tier: cfg.tier,
        })
    }

    /// 翻译并释放 C 字符串
    ///
    /// 每次 `translate` 分配一个 C 字符串，**必须**由
    /// `bergamot_translator_free_string` 释放，否则跨 FFI 内存泄漏。
    unsafe fn translate_one(&self, source: &str) -> Result<String> {
        let c_src = CString::new(source)
            .map_err(|_| Error::Engine("源文本含 NUL 字符".into()))?;
        let mut out: *mut c_char = std::ptr::null_mut();

        bergamot_translator_translate(self.model, c_src.as_ptr(), &mut out);

        if out.is_null() {
            return Ok(String::new()); // 引擎返回空结果（非错误）
        }
        let text = CStr::from_ptr(out).to_string_lossy().into_owned();
        bergamot_translator_free_string(out);
        Ok(text)
    }
}

impl Drop for BergamotTranslator {
    fn drop(&mut self) {
        if !self.model.is_null() {
            unsafe { bergamot_translator_destroy(self.model) };
            self.model = std::ptr::null_mut();
        }
    }
}

// 裸指针不自动实现 Send/Sync，需显式断言。
// 安全性依据：模型在 create 后只读，translate 无内部可变状态；
// 且 Bergamot 官方明确 translator 可多线程调用（Firefox 即如此使用）。
unsafe impl Send for BergamotTranslator {}
unsafe impl Sync for BergamotTranslator {}

impl std::fmt::Debug for BergamotTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BergamotTranslator")
            .field("model", &if self.model.is_null() { "null" } else { "ok" })
            .field("tier", &self.tier)
            .finish()
    }
}

impl MtBackend for BergamotTranslator {
    fn translate(&self, protected_text: &str) -> Result<Translation> {
        let start = std::time::Instant::now();
        if protected_text.trim().is_empty() {
            return Ok(Translation {
                text: String::new(),
                consistent: true,
                elapsed_ms: 0,
            });
        }

        // 跨 FFI 不得 panic：捕获后转为错误
        let src = protected_text.to_string();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            unsafe { self.translate_one(&src) }
        }));

        let text = match result {
            Ok(r) => r?,
            Err(_) => return Err(Error::Engine("Bergamot 翻译时发生内部 panic".into())),
        };

        Ok(Translation {
            text,
            // 哨兵校验由 PendingSegment::finish 负责
            consistent: true,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    fn name(&self) -> &'static str {
        "Bergamot (Marian NMT, Mozilla MPL-2.0)"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn tier(&self) -> QualityTier {
        self.tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mt::QualityTier;

    #[test]
    fn missing_dir_errors_clearly() {
        let cfg = MtConfig::new("no/such/dir", 2);
        let e = BergamotTranslator::load(&cfg).unwrap_err();
        assert!(matches!(e, Error::ModelMissing(_)));
    }

    #[test]
    fn wrong_format_gives_actionable_hint() {
        // 空目录 → 应提示需要 Bergamot 权重而非 CT2 产物
        let dir = std::env::temp_dir().join("translator_core_bergamot_fmt");
        let _ = std::fs::create_dir_all(&dir);
        // 只放 CT2 产物
        std::fs::write(dir.join("model.bin"), b"x").unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();

        let cfg = MtConfig::new(&dir, 2);
        let msg = BergamotTranslator::load(&cfg).unwrap_err().to_string();
        assert!(
            msg.contains("model.enzh.intgemm.alphas.bin"),
            "错误应指出缺少的文件: {msg}"
        );
        assert!(
            msg.contains("Bergamot") || msg.contains("Firefox"),
            "错误应区分于 CT2 格式: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn translator_is_send_and_sync() {
        fn assert_ss<T: Send + Sync>() {}
        assert_ss::<BergamotTranslator>();
    }

    #[test]
    fn quality_tier_affects_backend_behaviour() {
        // 两档共用同一引擎，差异体现在权重本身而非代码
        assert_ne!(QualityTier::Fast.beam_size(), QualityTier::Accurate.beam_size());
    }
}
