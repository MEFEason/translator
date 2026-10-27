//! 词库数据模型：所有输入格式归一化到这里的 `GlossaryFile`。
//!
//! 详见 docs/GLOSSARY.md §2。

use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;

/// 词条作用域
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// 仅作用于 ASR 热词偏置（Layer A）
    Asr,
    /// 仅作用于翻译前哨兵保护（Layer B/C）
    Mt,
    /// 两层都作用（默认）
    #[default]
    Both,
}

impl Scope {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "asr" => Scope::Asr,
            "mt" => Scope::Mt,
            _ => Scope::Both,
        }
    }

    pub fn applies_to_asr(&self) -> bool {
        matches!(self, Scope::Asr | Scope::Both)
    }

    pub fn applies_to_mt(&self) -> bool {
        matches!(self, Scope::Mt | Scope::Both)
    }
}

/// 单条词条
#[derive(Debug, Clone)]
pub struct Entry {
    /// 源词（英文），必填
    pub source: String,
    /// 目标译法（中文），必填
    pub target: String,
    /// 同义触发形式
    pub aliases: Vec<String>,
    /// 默认不区分大小写
    pub case_sensitive: bool,
    pub scope: Scope,
    pub group: Option<String>,
    /// 缺省 true
    pub enabled: bool,
    /// 备注，不参与翻译逻辑
    pub note: Option<String>,
    /// Layer C 兜底正则
    pub regex: Option<String>,
    /// 运行时注入：来源文件（不出现在词库文件中）
    pub source_file: PathBuf,
    /// 运行时注入：行号，用于错误定位
    pub line_no: u32,
}

impl Entry {
    /// 该词条参与 MT 保护的所有触发串：`source` + `aliases`
    pub fn mt_triggers(&self) -> Vec<String> {
        let mut v = vec![self.source.clone()];
        for a in &self.aliases {
            if !a.is_empty() && *a != self.source {
                v.push(a.clone());
            }
        }
        v
    }

    /// 该词条参与 ASR 热词的触发串
    pub fn asr_triggers(&self) -> Vec<String> {
        self.mt_triggers()
    }

    /// 匹配时使用的归一化键。
    ///
    /// 仅做 NFKC + 大小写折叠，**不折叠空白** ——
    /// 因为该结果要与原文的字节位置对齐用于切片，
    /// 空白折叠会改变字节数，破坏坐标映射。
    pub fn match_key(s: &str, case_sensitive: bool) -> String {
        let nfkc: String = s.nfkc().collect();
        if case_sensitive {
            nfkc
        } else {
            nfkc.to_lowercase()
        }
    }

    /// 存储键：进一步折叠空白，用于去重与合并。
    ///
    /// 与 `match_key` 分开是因为二者用途不同 —— 前者需保持与原文
    /// 坐标对齐，后者只用于相等性判断。
    pub fn storage_key(s: &str, case_sensitive: bool) -> String {
        let base = Self::match_key(s, case_sensitive);
        collapse_whitespace(&base)
    }

    /// 校验必填字段。返回 Err 时给出可定位的错误信息。
    pub fn validate(&self) -> Result<(), String> {
        if self.source.trim().is_empty() {
            return Err("source 不能为空".to_string());
        }
        if self.target.trim().is_empty() {
            return Err(format!("source '{}' 的 target 不能为空", self.source));
        }
        if let Some(r) = &self.regex {
            if !r.is_empty() {
                // 提前编译验证，避免运行时才发现正则非法
                if let Err(e) = compile_regex_check(r) {
                    return Err(format!("regex 非法: {e}"));
                }
            }
        }
        Ok(())
    }
}

/// 折叠连续空白为单空格，并去除首尾空白
pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 正则编译检查。这里用最小实现避免引入 regex 依赖到核心库，
/// 实际匹配在 `postfix.rs` 中由上层提供的正则引擎完成。
fn compile_regex_check(pattern: &str) -> Result<(), String> {
    // 基础健全性检查：括号配对
    let mut depth = 0i32;
    let mut escaped = false;
    for c in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err("括号不配对".to_string());
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("括号不配对".to_string());
    }
    Ok(())
}

/// 一个词库文件
#[derive(Debug, Clone, Default)]
pub struct GlossaryFile {
    pub version: u32,
    pub entries: Vec<Entry>,
}

/// 加载结果：成功条目 + 被跳过的警告（含定位）
#[derive(Debug, Clone, Default)]
pub struct LoadOutcome {
    pub file: PathBuf,
    pub accepted: usize,
    pub skipped: usize,
    pub warnings: Vec<(u32, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: &str, target: &str, scope: Scope) -> Entry {
        Entry {
            source: source.into(),
            target: target.into(),
            aliases: vec![],
            case_sensitive: false,
            scope,
            group: None,
            enabled: true,
            note: None,
            regex: None,
            source_file: PathBuf::from("test"),
            line_no: 1,
        }
    }

    #[test]
    fn match_key_preserves_whitespace_for_coordinate_alignment() {
        // match_key 不折叠空白 —— 因为匹配结果要切片原文，
        // 改变字节数会破坏坐标映射
        assert_eq!(
            Entry::match_key("  Kubernetes   Cluster ", false),
            "  kubernetes   cluster "
        );
        assert_eq!(
            Entry::match_key("Kubernetes   Cluster", true),
            "Kubernetes   Cluster"
        );
    }

    #[test]
    fn storage_key_folds_whitespace_for_dedup() {
        // storage_key 折叠空白，用于去重与合并
        assert_eq!(
            Entry::storage_key("  Kubernetes   Cluster ", false),
            "kubernetes cluster"
        );
        assert_eq!(
            Entry::storage_key("A  B", true),
            "A B"
        );
    }

    #[test]
    fn match_key_applies_nfkc() {
        // 全角 Ｋ  应与半角 K 归一化到同一键
        assert_eq!(
            Entry::match_key("Ｋｕｂｅｒｎｅｔｅｓ", false),
            Entry::match_key("Kubernetes", false)
        );
    }

    #[test]
    fn scope_parsing_is_case_insensitive_with_default() {
        assert_eq!(Scope::parse("ASR"), Scope::Asr);
        assert_eq!(Scope::parse(" mt "), Scope::Mt);
        assert_eq!(Scope::parse("nonsense"), Scope::Both);
        assert_eq!(Scope::parse(""), Scope::Both);
    }

    #[test]
    fn scope_applies_to_correct_layers() {
        assert!(Scope::Both.applies_to_asr());
        assert!(Scope::Both.applies_to_mt());
        assert!(Scope::Asr.applies_to_asr());
        assert!(!Scope::Asr.applies_to_mt());
        assert!(!Scope::Mt.applies_to_asr());
        assert!(Scope::Mt.applies_to_mt());
    }

    #[test]
    fn validate_rejects_empty_fields() {
        let mut e = entry("Kubernetes", "Kubernetes", Scope::Both);
        assert!(e.validate().is_ok());
        e.target = "  ".into();
        assert!(e.validate().is_err());
        e.source = "".into();
        assert!(e.validate().is_err());
    }

    #[test]
    fn validate_rejects_unbalanced_regex() {
        let mut e = entry("foo", "福", Scope::Both);
        e.regex = Some("foo(bar".into());
        assert!(e.validate().is_err());
        e.regex = Some("foo)(".into());
        assert!(e.validate().is_err());
        e.regex = Some("fo+bar".into());
        assert!(e.validate().is_ok());
    }

    #[test]
    fn triggers_include_aliases_without_duplicates() {
        let mut e = entry("Kubernetes", "Kubernetes", Scope::Both);
        e.aliases = vec!["k8s".into(), "Kubernetes".into()];
        let t = e.mt_triggers();
        assert_eq!(t, vec!["Kubernetes", "k8s"]);
    }

    #[test]
    fn collapse_whitespace_normalizes_all_kinds() {
        assert_eq!(collapse_whitespace("  a\t b \n c  "), "a b c");
        assert_eq!(collapse_whitespace("single"), "single");
        assert_eq!(collapse_whitespace("   "), "");
    }
}
