//! 机器翻译后端（en → zh）。
//!
//! ## 引擎选型：CTranslate2 还是 Bergamot
//!
//! 两者同为 Marian 框架，**权重（`opus-mt-en-zh`）与分词器
//! （`source.spm` / `target.spm`）完全复用**，翻译质量无差异。
//! 差别在构建方式，这是决定性因素：
//!
//! | | CTranslate2（Rust 绑定 2.1.1） | **Bergamot（选定）** |
//! |---|---|---|
//! | 构建方式 | `ctranslate2-src-build-support` **联网下载**预编译包 | 纯 CMake 源码编译 |
//! | 离线构建 | ❌ 代理 502 即失败 | ✅ 仅需 C++17 编译器 |
//! | 产物 | 动态库 | `libbergamot-translator.a` 静态库 |
//! | 依赖来源 | Mozilla（MPL-2.0） | Mozilla（MPL-2.0） |
//!
//! **本项目硬约束是"构建与运行双离线"**，故选 Bergamot。
//! 曾尝试 CTranslate2，其 `ureq`/`rustls`/`http` 出现在 build-dependency，
//! 被 `scripts/check_offline.py` 门禁拦截 —— 这是设计意图生效的证据。
//!
//! ## 离线性
//!
//! **运行期零网络**。权重与分词器均从本地模型目录读取；
//! 无任何 HTTP 客户端代码。构建期亦无需下载（纯源码编译）。

pub mod mock;

/// 空翻译后端（MT 可选模式：设备只装了 ASR 模型时）。
pub mod null;

#[cfg(feature = "mt-bergamot")]
pub mod bergamot;

use crate::error::Result;
use std::sync::Arc;

/// 翻译质量档位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityTier {
    /// int8 快档：延迟优先，桌面 ~250ms / Android ~800ms
    #[default]
    Fast,
    /// int8 base 精准档：通用场景更自然，延迟约翻倍
    Accurate,
}

impl QualityTier {
    pub fn beam_size(&self) -> usize {
        match self {
            // Marian 单语方向 beam=1 时质量下降明显，用 2 折中
            QualityTier::Fast => 2,
            QualityTier::Accurate => 4,
        }
    }

    pub fn max_decoding_length(&self) -> usize {
        match self {
            QualityTier::Fast => 128,
            QualityTier::Accurate => 256,
        }
    }

    pub fn compute_type(&self) -> &'static str {
        // 两档都用 int8。Accurate 档靠更大模型而非更高精度区分
        "int8"
    }
}

/// 翻译配置
#[derive(Debug, Clone)]
pub struct MtConfig {
    /// CT2 模型目录（含 model.bin / config.json / source.spm / target.spm）
    pub model_dir: std::path::PathBuf,
    pub tier: QualityTier,
    /// 推理线程数，来自 `AppConfig::mt_threads`
    pub num_threads: usize,
    /// 单次送入的最大 token 数。超过则按句子边界二次切分。
    pub max_input_tokens: usize,
}

impl MtConfig {
    pub fn new(model_dir: impl Into<std::path::PathBuf>, num_threads: usize) -> Self {
        Self {
            model_dir: model_dir.into(),
            tier: QualityTier::default(),
            num_threads,
            max_input_tokens: 96,
        }
    }
}

/// 一次翻译的结果
#[derive(Debug, Clone, PartialEq)]
pub struct Translation {
    /// 译文（哨兵已还原为词库译法）
    pub text: String,
    /// 哨兵往返校验是否通过。false → 需 Layer C 兜底 + UI 标记
    pub consistent: bool,
    /// 推理耗时（毫秒），用于健康度上报
    pub elapsed_ms: u64,
}

/// 翻译引擎。
///
/// 契约：`translate` 接收**已做哨兵保护**的文本，产出**未还原哨兵**的译文。
/// 哨兵还原由 `glossary::snapshot::PendingSegment::finish` 负责 ——
/// 这样词库机制与翻译引擎完全解耦，换引擎不影响三层机制。
pub trait MtBackend: Send + Sync {
    fn translate(&self, protected_text: &str) -> Result<Translation>;

    /// 批量翻译。默认逐条调用（真实实现应覆写以利用 batch 加速）。
    fn translate_batch(&self, texts: &[String]) -> Result<Vec<Translation>> {
        texts.iter().map(|t| self.translate(t)).collect()
    }

    fn name(&self) -> &'static str;
    fn is_available(&self) -> bool;
    fn unavailable_reason(&self) -> Option<String> {
        None
    }
    fn tier(&self) -> QualityTier;
}

/// 共享翻译器句柄
pub type SharedMt = Arc<dyn MtBackend>;

/// 按 token 数粗估切分点，把过长文本按句边界拆开。
///
/// Marian 上下文 512 token，但实时场景单段通常远小于此。
/// 超限时切分可避免整段丢失 —— 直接截断会产出半句话译文。
pub fn split_by_sentence(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();

    // 优先在句末标点处断开
    for piece in split_keeping_delims(text) {
        if cur.chars().count() + piece.chars().count() > max_chars && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push_str(piece);
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    if parts.is_empty() {
        vec![text.to_string()]
    } else {
        parts
    }
}

/// 按中英文句末标点切分，保留标点
fn split_keeping_delims(text: &str) -> Vec<&str> {
    const SENTENCE_DELIMS: [char; 6] = ['。', '！', '？', '.', '!', '?'];
    const CLAUSE_DELIMS: [char; 3] = ['，', ',', ';'];

    let mut out = Vec::new();
    let mut start = 0usize;
    let mut pending_clause: Option<usize> = None;
    let mut last_sentence: Option<usize> = None;

    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();

        if SENTENCE_DELIMS.contains(&c) {
            last_sentence = Some(end);
            // 句末标点后若有空格，一并纳入
            if text[end..].starts_with(' ') {
                last_sentence = Some(end + 1);
            }
        }

        if CLAUSE_DELIMS.contains(&c) {
            pending_clause = Some(end);
        }

        // 片段已够长（≥40 字符）时，找最近的断点切开
        if i + c.len_utf8() - start >= 40 {
            if let Some(cut) = last_sentence.filter(|&d| d > start).or(pending_clause) {
                out.push(&text[start..cut]);
                start = cut;
            }
            last_sentence = None;
            pending_clause = None;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    if out.is_empty() {
        out.push(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_not_split() {
        let r = split_by_sentence("hello world", 100);
        assert_eq!(r, vec!["hello world"]);
    }

    #[test]
    fn long_text_split_at_sentence_boundary() {
        let long = "First sentence here. Second sentence follows. Third one now.";
        let r = split_by_sentence(long, 30);
        assert!(r.len() > 1, "长文本应被切分");
        for part in &r {
            assert!(part.chars().count() <= 45, "片段过长: {part}");
        }
    }

    #[test]
    fn split_preserves_all_content() {
        let text = "Alpha beta gamma. Delta epsilon zeta. Eta theta iota.";
        let r = split_by_sentence(text, 25);
        let joined: String = r.concat();
        assert_eq!(joined, text, "切分不得丢内容");
    }

    #[test]
    fn split_handles_chinese_punctuation() {
        let text = "这是第一句话。这是第二句话。这是第三句话。";
        let r = split_by_sentence(text, 12);
        assert!(r.len() > 1);
        assert_eq!(r.concat(), text);
    }

    #[test]
    fn split_single_no_delimiter_long_text_uses_comma() {
        let text = "aaa, bbb, ccc, ddd, eee, fff, ggg, hhh, iii, jjj, kkk, lll";
        let r = split_by_sentence(text, 40);
        assert!(r.len() > 1);
        assert_eq!(r.concat(), text);
    }

    #[test]
    fn tier_maps_to_params() {
        assert_eq!(QualityTier::Fast.beam_size(), 2);
        assert_eq!(QualityTier::Accurate.beam_size(), 4);
        assert_eq!(QualityTier::Fast.max_decoding_length(), 128);
        assert_eq!(QualityTier::Accurate.compute_type(), "int8");
    }

    #[test]
    fn empty_input_returns_single_empty() {
        let r = split_by_sentence("", 10);
        assert_eq!(r.len(), 1);
    }
}
