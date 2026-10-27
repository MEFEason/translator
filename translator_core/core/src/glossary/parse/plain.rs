//! 纯文本解析（最简格式：每行 `source` + 制表符或多个空格 + `target`）。
//!
//! 面向"从别处批量粘贴"的场景 —— 用户不需要学任何格式。

use super::{finalize, make_entry, ParsedFile};
use crate::error::Result;
use std::path::PathBuf;

pub fn parse(text: &str, file: PathBuf) -> Result<ParsedFile> {
    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let mut line_no = 0u32;

    for raw in text.lines() {
        line_no += 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // 制表符优先；否则用 2 个以上空格分隔
        let (source, target) = if let Some((s, t)) = line.split_once('\t') {
            (s.trim(), t.trim())
        } else {
            match line.split_once("  ") {
                Some((s, t)) => (s.trim(), t.trim()),
                None => continue, // 无分隔符，跳过
            }
        };

        if source.is_empty() || target.is_empty() {
            continue;
        }
        entries.push(make_entry(&file, line_no, source.to_string(), target.to_string()));
    }

    let entries = finalize(entries, &mut warnings);
    Ok(ParsedFile { version: 1, entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("test.txt")
    }

    #[test]
    fn tab_separated() {
        let r = parse("Kubernetes\tKubernetes\ngRPC\tgRPC", p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].source, "Kubernetes");
        assert_eq!(r.entries[0].target, "Kubernetes");
    }

    #[test]
    fn multi_space_separated() {
        let r = parse("Kubernetes    Kubernetes\nReact    React", p()).unwrap();
        assert_eq!(r.entries.len(), 2);
    }

    #[test]
    fn lines_without_separator_skipped() {
        let r = parse("Kubernetes\tKubernetes\ngarbage line", p()).unwrap();
        assert_eq!(r.entries.len(), 1);
    }

    #[test]
    fn skips_comments_and_blanks() {
        let r = parse("# header\n\nKubernetes\tKubernetes\n", p()).unwrap();
        assert_eq!(r.entries.len(), 1);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let r = parse("  Kubernetes  \t  Kubernetes  ", p()).unwrap();
        assert_eq!(r.entries[0].source, "Kubernetes");
        assert_eq!(r.entries[0].target, "Kubernetes");
    }

    #[test]
    fn single_space_is_not_a_separator() {
        // 单空格不足以分隔，避免误把整行当 source
        let r = parse("Kubernetes Kubernetes", p()).unwrap();
        assert_eq!(r.entries.len(), 0);
    }
}
