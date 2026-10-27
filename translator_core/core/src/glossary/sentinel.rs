//! Layer B：翻译前哨兵保护。
//!
//! ## 为什么需要这一层
//!
//! 直接翻译 `We deployed Kubernetes yesterday`，Marian 会输出
//! `我们昨天部署了库伯内特斯` —— 无论译文本身多准确，术语都会被意译。
//! 靠"喂术语表给翻译模型"解决不了这个问题，因为 Marian 没有术语约束接口。
//!
//! ## 做法
//!
//! 1. 用 Aho-Corasick 在源文本上做**最长优先非重叠匹配**。
//! 2. 命中的 span 替换为哨兵 `▁X{n}X`。
//! 3. 送入翻译引擎 —— Marian 的 SentencePiece 会把 `▁` 当作词边界前缀，
//!    这串字符被切成独立的未登录片段，模型不会尝试翻译它，原样透传。
//! 4. 输出中按索引还原为词库指定的译法。
//!
//! 选 `▁X{n}X` 这个形式是关键：`▁`(U+2581) 是 SentencePiece 的显式词边界
//! 标记，保证哨兵不会被粘连到相邻词上；`X{n}` 是数字，确保不会与自然语言冲突。

use crate::glossary::schema::Entry;
use aho_corasick::{AhoCorasick, MatchKind};

/// 哨兵模板：`▁X{index}X`
const SENTINEL_PREFIX: &str = "\u{2581}X";
const SENTINEL_SUFFIX: &str = "X";

/// 一次替换的结果
#[derive(Debug, Clone, Default)]
pub struct SentinelPass {
    /// 替换后的文本，可直接送入翻译引擎
    pub text: String,
    /// 哨兵索引 → 目标译法
    pub mapping: Vec<String>,
    /// 命中的词条索引（用于统计）
    pub hit_entries: Vec<usize>,
}

impl SentinelPass {
    pub fn sentinel_count(&self) -> usize {
        self.mapping.len()
    }
}

/// 构建用于 MT 层的匹配自动机。
///
/// 只收录 `scope` 作用于 MT 且 `enabled` 的词条。
/// 词条索引即其在 `entries` 中的位置，供统计使用。
pub struct GlossaryMatcher {
    /// Aho-Corasick 自动机。pattern 与 `patterns` 一一对应。
    ac: AhoCorasick,
    /// 归一化后的匹配键（与 ac 的 pattern 同序）
    patterns: Vec<String>,
    /// pattern → 词条在 entries 中的下标
    pattern_to_entry: Vec<usize>,
    /// 词条下标 → 目标译法。
    ///
    /// matcher 自持译法表（而非引用 `&[Entry]`），因此
    /// `GlossarySnapshot` 可自由 `Send`/`Sync` 并跨线程使用 ——
    /// 这对 §6.1 的线程拓扑是硬约束。
    entry_targets: Vec<String>,
}

impl std::fmt::Debug for GlossaryMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlossaryMatcher")
            .field("patterns", &self.patterns.len())
            .field("entries", &self.entry_targets.len())
            .finish()
    }
}

impl GlossaryMatcher {
    /// `entries`：完整词条数组（快照内）
    pub fn build(entries: &[Entry]) -> Self {
        let mut patterns: Vec<String> = Vec::new();
        let mut pattern_to_entry: Vec<usize> = Vec::new();
        let entry_targets: Vec<String> = entries.iter().map(|e| e.target.clone()).collect();

        for (idx, e) in entries.iter().enumerate() {
            if !e.enabled || !e.scope.applies_to_mt() {
                continue;
            }
            for trigger in e.mt_triggers() {
                let key = Entry::match_key(&trigger, e.case_sensitive);
                if key.is_empty() {
                    continue;
                }
                patterns.push(key);
                pattern_to_entry.push(idx);
            }
        }

        // 必须用 `LeftmostLongest`：同起点存在多个模式时（如 "Java" 与
        // "JavaScript"），`Standard` 只报告其中一个（实测为最短），
        // 导致长术语被短前缀吃掉 —— "JavaScript" 会被切成 "Java" + "Script"。
        // `LeftmostLongest` 的语义正是「最左匹配优先，同位取最长」。
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .expect("构建 Aho-Corasick 自动机失败");

        Self {
            ac,
            patterns,
            pattern_to_entry,
            entry_targets,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    /// 对文本执行哨兵替换。
    ///
    /// 注意：匹配在归一化文本上进行，但**切片输出来自原文**。
    /// 否则会把 "We run Kubernetes" 的 "We" 变成 "we"，
    /// 破坏原文大小写，直接影响译文质量与字幕观感。
    pub fn protect(&self, text: &str) -> SentinelPass {
        if self.is_empty() || text.is_empty() {
            return SentinelPass {
                text: text.to_string(),
                mapping: Vec::new(),
                hit_entries: Vec::new(),
            };
        }

        // 归一化文本仅用于匹配定位
        let normalized = Entry::match_key(text, false);

        // 收集所有命中区间
        let mut spans: Vec<(usize, usize, usize)> = self
            .ac
            .find_iter(&normalized)
            .map(|m| (m.start(), m.end(), m.pattern().as_usize()))
            .collect();

        if spans.is_empty() {
            return SentinelPass {
                text: text.to_string(),
                mapping: Vec::new(),
                hit_entries: Vec::new(),
            };
        }

        // 归一化可能改变长度（NFKC、大小写折叠为 1 字节 ASCII），
        // 故需把归一化坐标映射回原文坐标。
        let orig_index = build_index_map(text);

        // `LeftmostLongest` 已保证「不重叠 + 同位取最长」，
        // 故按起点排序后直接顺序接纳即可。
        spans.sort_by(|a, b| a.0.cmp(&b.0));

        let mut accepted: Vec<(usize, usize, usize)> = Vec::with_capacity(spans.len());
        let mut last_end = 0usize;
        for (start, end, pattern) in spans {
            if start < last_end {
                continue; // 与已接受的区间重叠，丢弃
            }
            accepted.push((start, end, pattern));
            last_end = end;
        }

        // 生成替换结果：切片取自原文，保留原始大小写与格式
        let mut out = String::with_capacity(text.len() + 32);
        let mut mapping: Vec<String> = Vec::with_capacity(accepted.len());
        let mut hit_entries: Vec<usize> = Vec::with_capacity(accepted.len());
        let mut cursor = 0usize;

        for (start, end, pattern) in accepted {
            let entry_idx = self.pattern_to_entry[pattern];
            let Some(target) = self.entry_targets.get(entry_idx) else {
                continue;
            };
            // 归一化坐标 → 原文边界坐标
            let (Some(&os), Some(&oe)) = (orig_index.get(start), orig_index.get(end)) else {
                continue; // 坐标异常则跳过，不产出错误结果
            };
            if os < cursor || oe > text.len() || os > oe {
                continue;
            }
            let sentinel = format!("{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}", mapping.len());
            out.push_str(&text[cursor..os]);
            out.push_str(&sentinel);
            mapping.push(target.clone());
            hit_entries.push(entry_idx);
            cursor = oe;
        }
        out.push_str(&text[cursor..]);

        SentinelPass {
            text: out,
            mapping,
            hit_entries,
        }
    }
}

/// 建立「归一化字节下标 → 原文字节下标」的映射。
///
/// 返回数组长度 = 归一化文本长度 + 1。
/// 语义：`map[i]` = 归一化文本第 `i` 个字节**在原文中的位置**。
/// `map[normalized_len]` = 原文长度（末尾哨兵）。
///
/// NFKC 与大小写折叠会改变字节长度，故归一化坐标不能直接切原文。
/// 本函数按字符逐一推进：字符 `ch` 在归一化后产生 n 个字节，
/// 这 n 个位置全部映射到 `ch` 在原文中的**起始偏移**；
/// 再为下一字符预留其后继位置的映射。
///
/// 例：`"AB"` → 归一化 `"ab"`。
/// map = [0, 1, 2]（长度 3 = 2 + 1 末尾哨兵）
/// 归一化 (0,1) → 原文 [0..1] = "A"  归一化 (1,2) → 原文 [1..2] = "B"
fn build_index_map(text: &str) -> Vec<usize> {
    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
    let mut orig_byte = 0usize;
    for ch in text.chars() {
        let normalized: String = Entry::match_key(&ch.to_string(), false);
        for _ in normalized.bytes() {
            map.push(orig_byte);
        }
        orig_byte += ch.len_utf8();
    }
    // 末尾哨兵：归一化文本的结束位置对应原文长度
    map.push(orig_byte);
    map
}

/// 哨兵还原。
///
/// 返回还原结果与一致性校验（还原的哨兵数是否等于替换数）。
/// 不一致时上层应触发 Layer C 兜底并标记该段待复核。
#[derive(Debug, Clone)]
pub struct RestorePass {
    pub text: String,
    pub restored: usize,
    pub expected: usize,
    /// 出现但索引越界的哨兵（理论上不应发生，除非 SnapshotGuard 被绕过）
    pub orphan_sentinels: usize,
}

impl RestorePass {
    pub fn is_consistent(&self) -> bool {
        self.restored == self.expected && self.orphan_sentinels == 0
    }
}

/// 从译文中还原哨兵。
pub fn restore(text: &str, mapping: &[String]) -> RestorePass {
    if mapping.is_empty() {
        return RestorePass {
            text: text.to_string(),
            restored: 0,
            expected: 0,
            orphan_sentinels: 0,
        };
    }

    let mut out = String::with_capacity(text.len());
    let mut restored = 0usize;
    let mut orphans = 0usize;
    let mut i = 0usize;
    let bytes = text.as_bytes();

    while i < text.len() {
        if text[i..].starts_with(SENTINEL_PREFIX) {
            // 读取数字
            let mut j = i + SENTINEL_PREFIX.len();
            let num_start = j;
            while j < text.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > num_start && text[j..].starts_with(SENTINEL_SUFFIX) {
                let idx: usize = text[num_start..j].parse().unwrap_or(usize::MAX);
                if let Some(t) = mapping.get(idx) {
                    out.push_str(t);
                    restored += 1;
                    i = j + SENTINEL_SUFFIX.len();
                    continue;
                } else {
                    orphans += 1;
                }
            }
        }
        // 非哨兵字符：按字符边界推进，避免切碎多字节 UTF-8
        let ch_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }

    RestorePass {
        text: out,
        restored,
        expected: mapping.len(),
        orphan_sentinels: orphans,
    }
}

/// 由 snapshot 在构建 matcher 后调用，注入目标译法表。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::glossary::schema::Scope;
    use std::path::PathBuf;

    fn mk(source: &str, target: &str, aliases: Vec<&str>) -> Entry {
        Entry {
            source: source.into(),
            target: target.into(),
            aliases: aliases.into_iter().map(String::from).collect(),
            case_sensitive: false,
            scope: Scope::Both,
            group: None,
            enabled: true,
            note: None,
            regex: None,
            source_file: PathBuf::from("t"),
            line_no: 1,
        }
    }

    /// 构建 matcher
    fn make(entries: &[Entry]) -> GlossaryMatcher {
        GlossaryMatcher::build(entries)
    }

    #[test]
    fn matcher_is_send_and_sync() {
        // 硬约束：快照需跨线程使用（§6.1 线程拓扑）
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GlossaryMatcher>();
    }

    #[test]
    fn term_is_replaced_by_sentinel() {
        let entries = vec![mk("Kubernetes", "Kubernetes", vec![])];
        let m = make(&entries);
        let r = m.protect("We deployed Kubernetes yesterday");
        assert_eq!(r.text, "We deployed \u{2581}X0X yesterday");
        assert_eq!(r.mapping, vec!["Kubernetes"]);
        assert_eq!(r.hit_entries, vec![0]);
    }

    #[test]
    fn roundtrip_restores_original_target() {
        let entries = vec![mk("Kubernetes", "Kubernetes", vec![])];
        let m = make(&entries);
        let original = "We deployed Kubernetes yesterday";
        let pass = m.protect(original);
        assert_eq!(pass.text, "We deployed \u{2581}X0X yesterday");

        // 模拟 Marian 输出：哨兵原样透传，中文语序重排
        let mt_output = "我们昨天部署了\u{2581}X0X";
        let r = restore(mt_output, &pass.mapping);
        assert!(
            r.is_consistent(),
            "expected={} restored={} orphans={}",
            r.expected,
            r.restored,
            r.orphan_sentinels
        );
        assert_eq!(r.text, "我们昨天部署了Kubernetes");
    }

    #[test]
    fn multiple_terms_get_distinct_sentinels() {
        let entries = vec![
            mk("Kubernetes", "Kubernetes", vec![]),
            mk("gRPC", "gRPC", vec![]),
            mk("React", "React", vec![]),
        ];
        let m = make(&entries);
        let r = m.protect("Kubernetes and gRPC and React");
        assert_eq!(r.sentinel_count(), 3);
        assert!(r.text.contains("\u{2581}X0X"));
        assert!(r.text.contains("\u{2581}X1X"));
        assert!(r.text.contains("\u{2581}X2X"));
    }

    #[test]
    fn alias_triggers_match() {
        let entries = vec![mk("Kubernetes", "Kubernetes", vec!["k8s", "K8s"])];
        let m = make(&entries);
        let r = m.protect("we run k8s in prod");
        assert_eq!(r.sentinel_count(), 1);
    }

    #[test]
    fn overlapping_matches_pick_longest() {
        // "Java" 与 "JavaScript" 重叠，应只命中更长的
        let entries = vec![
            mk("Java", "Java", vec![]),
            mk("JavaScript", "JavaScript", vec![]),
        ];
        let m = make(&entries);
        let r = m.protect("JavaScript is great");
        assert_eq!(r.sentinel_count(), 1);
        assert_eq!(r.mapping, vec!["JavaScript"]);
    }

    #[test]
    fn case_insensitive_by_default() {
        let entries = vec![mk("Kubernetes", "Kubernetes", vec![])];
        let m = make(&entries);
        assert_eq!(m.protect("KUBERNETES is great").sentinel_count(), 1);
    }

    #[test]
    fn no_match_returns_original_unchanged() {
        let entries = vec![mk("Kubernetes", "Kubernetes", vec![])];
        let m = make(&entries);
        let r = m.protect("nothing relevant here");
        assert_eq!(r.text, "nothing relevant here");
        assert_eq!(r.sentinel_count(), 0);
    }

    #[test]
    fn restore_detects_orphan_sentinel() {
        // mapping 只有 1 项，但译文里出现 X5X → 越界
        let mapping = vec!["Kubernetes".to_string()];
        let r = restore("我们用 \u{2581}X5X 部署", &mapping);
        assert!(!r.is_consistent());
        assert_eq!(r.orphan_sentinels, 1);
    }

    #[test]
    fn restore_detects_missing_sentinel() {
        // mapping 有 2 项，译文里只剩 1 个 → 不一致
        let mapping = vec!["A".to_string(), "B".to_string()];
        let r = restore("we use \u{2581}X0X here", &mapping);
        assert!(!r.is_consistent());
        assert_eq!(r.restored, 1);
        assert_eq!(r.expected, 2);
    }

    #[test]
    fn restore_preserves_chinese_text() {
        let mapping = vec!["Kubernetes".to_string()];
        let r = restore("我们在生产环境使用\u{2581}X0X部署服务", &mapping);
        assert_eq!(r.text, "我们在生产环境使用Kubernetes部署服务");
    }

    #[test]
    fn empty_mapping_is_passthrough() {
        let r = restore("原文不变", &[]);
        assert_eq!(r.text, "原文不变");
        assert!(r.is_consistent());
    }

    #[test]
    fn disabled_entries_are_excluded() {
        let mut e = mk("Kubernetes", "Kubernetes", vec![]);
        e.enabled = false;
        let entries = vec![e];
        let m = make(&entries);
        assert!(m.is_empty());
        assert_eq!(m.protect("Kubernetes").sentinel_count(), 0);
    }

    #[test]
    fn asr_only_entries_excluded_from_mt_matcher() {
        let mut e = mk("Kubernetes", "Kubernetes", vec![]);
        e.scope = Scope::Asr;
        let entries = vec![e];
        let m = make(&entries);
        assert!(m.is_empty(), "scope=Asr 的词条不应参与 MT 哨兵");
    }

    #[test]
    fn mt_only_entries_excluded_from_asr_but_in_matcher() {
        let mut e = mk("i.e.", "也就是说", vec![]);
        e.scope = Scope::Mt;
        let entries = vec![e];
        let m = make(&entries);
        assert_eq!(m.protect("i.e. means that").sentinel_count(), 1);
    }

    #[test]
    fn multibyte_text_survives_restore() {
        // 校验 restore 不切碎多字节字符（emoji + 中文）
        let mapping = vec!["术语".to_string()];
        let r = restore("前缀🎉中文\u{2581}X0X后缀", &mapping);
        assert_eq!(r.text, "前缀🎉中文术语后缀");
    }
}
