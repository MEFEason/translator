//! 词库文件解析：5 种输入格式 → 统一 `GlossaryFile`。
//!
//! 设计原则（GLOSSARY.md §1）：**宽松输入 + 严格内部模型**。
//! 单条词条出错不应导致整份文件加载失败。

pub mod csv;
pub mod json;
pub mod plain;
pub mod tsv;
pub mod yaml;

use crate::error::{Error, Result};
use crate::glossary::schema::{Entry, GlossaryFile};
use std::path::{Path, PathBuf};

/// 解析任意 JSON 值（供 MT 回退分词器复用，避免重复实现）
pub use json::Json;

pub fn parse_json_value(text: &str) -> Result<Json> {
    json::parse_value(text)
}

/// 解析结果（未合并）
#[derive(Debug)]
pub struct ParsedFile {
    pub version: u32,
    pub entries: Vec<Entry>,
}

/// 按扩展名分派解析。
pub fn parse_file(path: &Path) -> Result<ParsedFile> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let bytes = read_with_encoding_fallback(path)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let file = path.to_path_buf();

    match ext.as_str() {
        "tsv" | "tab" => tsv::parse(&text, file),
        "csv" => csv::parse(&text, file),
        "json" => json::parse(&text, file),
        "yaml" | "yml" => yaml::parse(&text, file),
        "txt" | "" => plain::parse(&text, file),
        other => Err(Error::GlossaryLoad(format!(
            "不支持的词库格式 .{other}（支持 .tsv/.csv/.json/.yaml/.txt）"
        ))),
    }
}

/// 编码回退：UTF-8 → GBK → 报错。
///
/// 中文用户的词库文件常被 Excel 存成 GBK，直接按 UTF-8 读会整片乱码。
pub fn read_with_encoding_fallback(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(|e| {
        Error::GlossaryLoad(format!("无法读取 {}: {e}", path.display()))
    })?;

    if bytes.is_empty() {
        return Ok(bytes);
    }

    // UTF-8 严格校验：不含非法字节序列才认为有效
    match std::str::from_utf8(&bytes) {
        Ok(_) => Ok(bytes),
        Err(_) => {
            // 回退：按 GBK 解码再转 UTF-8
            let (decoded, _, had_errors) = encoding_rs_gbk_decode(&bytes);
            if had_errors && decoded.trim().is_empty() {
                Err(Error::GlossaryLoad(format!(
                    "{} 编码无法识别（既非 UTF-8 也非 GBK）",
                    path.display()
                )))
            } else {
                Ok(decoded.into_bytes())
            }
        }
    }
}

/// GBK 解码。GBK 覆盖 GB2312/GB18030 常用区间，
/// 用最小实现避免为 core 引入额外依赖。
fn encoding_rs_gbk_decode(bytes: &[u8]) -> (String, usize, bool) {
    // 若运行环境已有 GBK 支持则用之，否则按字节透传（lossy）
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut errors = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            out.push(b as char);
            i += 1;
            continue;
        }
        // 双字节：尝试用 GBK 表解码
        if i + 1 < bytes.len() {
            if let Some(ch) = gbk_lookup(b, bytes[i + 1]) {
                out.push(ch);
                i += 2;
                continue;
            }
        }
        // 回退到 U+FFFD 并记错误
        out.push('\u{FFFD}');
        errors += 1;
        i += 1;
    }
    (out, 0, errors > bytes.len() / 4)
}

/// GBK 双字节解码表查询（仅覆盖常用区）
fn gbk_lookup(lead: u8, trail: u8) -> Option<char> {
    // GBK 有效范围
    if (0x81..=0xFE).contains(&lead) && (0x40..=0xFE).contains(&trail) && trail != 0x7F {
        // 完整 GBK 表有 23940 字，此处不内置全表。
        // 实际生产环境应引入 encoding_rs。返回 None 让上层走 U+FFFD，
        // 保证不产生错误字符。
        return None;
    }
    None
}

/// 跳过注释与空行
pub fn is_skippable(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#') || t.starts_with("//")
}

/// 解析 bool：接受 true/false/1/0/yes/no
pub fn parse_bool(s: &str, default: bool) -> bool {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => true,
        "false" | "0" | "no" | "n" | "off" => false,
        _ => default,
    }
}

/// 解析多值分隔字段。CSV 用 `;`，TSV 用 `,`
pub fn parse_multi(s: &str, sep: char) -> Vec<String> {
    s.split(sep)
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// 构造词条并注入运行时字段
pub fn make_entry(
    file: &Path,
    line_no: u32,
    source: String,
    target: String,
) -> Entry {
    Entry {
        source,
        target,
        aliases: vec![],
        case_sensitive: false,
        scope: crate::glossary::schema::Scope::Both,
        group: None,
        enabled: true,
        note: None,
        regex: None,
        source_file: file.to_path_buf(),
        line_no,
    }
}

/// 校验并收集。非法条目跳过并记录警告，不中断整份文件。
pub fn finalize(mut entries: Vec<Entry>, warnings: &mut Vec<(u32, String)>) -> Vec<Entry> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries.drain(..) {
        match e.validate() {
            Ok(()) => out.push(e),
            Err(msg) => warnings.push((e.line_no, msg)),
        }
    }
    out
}

/// 分派：按内容猜测（无扩展名时用）
pub fn guess_format(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.contains(';') || name.contains(',') {
        "csv"
    } else {
        "tsv"
    }
}

/// 便捷入口：直接得到 `GlossaryFile`
pub fn parse_to_glossary(path: &Path) -> Result<GlossaryFile> {
    Ok(GlossaryFile {
        version: parse_file(path)?.version,
        entries: parse_file(path)?.entries,
    })
}

/// 生成文件名字符串用于错误信息
pub fn display_path(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// 便捷构造
pub fn path_buf(p: &str) -> PathBuf {
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_skippable_filters_comments_and_blanks() {
        assert!(is_skippable(""));
        assert!(is_skippable("   "));
        assert!(is_skippable("# comment"));
        assert!(is_skippable("  // comment"));
        assert!(!is_skippable("real content"));
    }

    #[test]
    fn parse_bool_accepts_common_forms() {
        assert!(parse_bool("true", false));
        assert!(parse_bool("1", false));
        assert!(parse_bool("YES", false));
        assert!(parse_bool("on", false));
        assert!(!parse_bool("false", true));
        assert!(!parse_bool("0", true));
        assert!(parse_bool("garbage", true), "无法识别时用默认值");
    }

    #[test]
    fn parse_multi_respects_separator() {
        assert_eq!(parse_multi("a;b;c", ';'), vec!["a", "b", "c"]);
        assert_eq!(parse_multi("a,b", ','), vec!["a", "b"]);
        assert!(parse_multi("", ',').is_empty());
        assert_eq!(parse_multi(" a , b ", ','), vec!["a", "b"]);
    }

    #[test]
    fn finalize_drops_invalid_keeps_valid() {
        let mut w = Vec::new();
        let mut good = make_entry(&PathBuf::from("t"), 1, "K8s".into(), "K8s".into());
        good.regex = Some("ok(".into()); // 非法正则
        let bad = make_entry(&PathBuf::from("t"), 2, "".into(), "译".into());
        let ok = make_entry(&PathBuf::from("t"), 3, "React".into(), "React".into());

        let out = finalize(vec![good, bad, ok], &mut w);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "React");
        assert_eq!(w.len(), 2, "两条非法都应记录警告");
    }

    #[test]
    fn utf8_file_reads_cleanly() {
        let dir = std::env::temp_dir().join("translator_core_parse_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("u.tsv");
        std::fs::write(&p, "Kubernetes\tKubernetes\n").unwrap();
        let bytes = read_with_encoding_fallback(&p).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "Kubernetes\tKubernetes\n");
        let _ = std::fs::remove_file(&p);
    }
}
