//! TSV 解析（首选格式）。
//!
//! - `#` 开头为注释
//! - 首行若为表头则按列名映射，否则按位置读取
//! - 列序（无表头时）：source / target / aliases / scope / group / enabled / note

use super::{finalize, is_skippable, make_entry, parse_bool, parse_multi, ParsedFile};
use crate::error::Result;
use crate::glossary::schema::Scope;
use std::path::PathBuf;

const HEADER_CANDS: [&str; 7] = [
    "source", "target", "aliases", "scope", "group", "enabled", "note",
];

pub fn parse(text: &str, file: PathBuf) -> Result<ParsedFile> {
    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let mut columns: Option<Vec<usize>> = None;
    let mut line_no = 0u32;

    for raw in text.lines() {
        line_no += 1;
        if is_skippable(raw) {
            continue;
        }

        let cells: Vec<&str> = raw.split('\t').map(|c| c.trim()).collect();

        // 表头检测：至少含 source 与 target
        if columns.is_none() {
            let lower: Vec<String> = cells.iter().map(|c| c.to_ascii_lowercase()).collect();
            if lower.iter().any(|c| c == "source") && lower.iter().any(|c| c == "target") {
                let mut cols = vec![usize::MAX; HEADER_CANDS.len()];
                for (ci, name) in lower.iter().enumerate() {
                    if let Some(hi) = HEADER_CANDS.iter().position(|h| h == name) {
                        cols[hi] = ci;
                    }
                }
                columns = Some(cols);
                continue;
            }
            // 无表头 → 使用默认列序
            columns = Some(vec![0usize, 1, 2, 3, 4, 5, 6]);
        }

        let cols = columns.as_ref().unwrap();
        let get = |i: usize| -> &str {
            cols.get(i)
                .filter(|&&c| c != usize::MAX)
                .and_then(|&c| cells.get(c))
                .copied()
                .unwrap_or("")
        };

        // TSV 内 aliases 用逗号分隔多值（不用 `;`，避免与 CSV 混淆）
        let mut e = make_entry(&file, line_no, get(0).to_string(), get(1).to_string());
        e.aliases = parse_multi(get(2), ',');
        e.scope = Scope::parse(get(3));
        let g = get(4);
        if !g.is_empty() {
            e.group = Some(g.to_string());
        }
        e.enabled = parse_bool(get(5), true);
        let n = get(6);
        if !n.is_empty() {
            e.note = Some(n.to_string());
        }
        entries.push(e);
    }

    let entries = finalize(entries, &mut warnings);
    if !warnings.is_empty() {
        log_warnings(&file, &warnings);
    }
    Ok(ParsedFile { version: 1, entries })
}

/// 暂用 eprintln，替换为 UI 上报通道
fn log_warnings(file: &PathBuf, warnings: &[(u32, String)]) {
    for (line, msg) in warnings {
        eprintln!("[glossary] {}:{} {}", file.display(), line, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("test.tsv")
    }

    #[test]
    fn parses_positional_without_header() {
        let t = "Kubernetes\tKubernetes\ngRPC\tgRPC\nReact\tReact";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 3);
        assert_eq!(r.entries[0].source, "Kubernetes");
        assert_eq!(r.entries[1].target, "gRPC");
    }

    #[test]
    fn parses_with_header_any_order() {
        let t = "target\tsource\tgroup\nKubernetes\tKubernetes\t技术\nReact\tReact\t前端";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].source, "Kubernetes");
        assert_eq!(r.entries[0].target, "Kubernetes");
        assert_eq!(r.entries[0].group.as_deref(), Some("技术"));
    }

    #[test]
    fn parses_aliases_scope_enabled_note() {
        let t = "Kubernetes\tKubernetes\tk8s,K8s\tasr\t技术\tfalse\t官方名称";
        let r = parse(t, p()).unwrap();
        let e = &r.entries[0];
        assert_eq!(e.aliases, vec!["k8s", "K8s"]);
        assert_eq!(e.scope, Scope::Asr);
        assert_eq!(e.group.as_deref(), Some("技术"));
        assert!(!e.enabled);
        assert_eq!(e.note.as_deref(), Some("官方名称"));
    }

    #[test]
    fn skips_comments_and_blanks() {
        let t = "# comment\n\nKubernetes\tKubernetes\n\n// another\nReact\tReact";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
    }

    #[test]
    fn drops_empty_target_but_keeps_others() {
        let t = "Kubernetes\t\nReact\tReact";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].source, "React");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let t = "Kubernetes\tKubernetes\r\nReact\tReact\r\n";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].target, "Kubernetes");
    }

    #[test]
    fn missing_optional_columns_use_defaults() {
        let t = "Kubernetes\tKubernetes";
        let r = parse(t, p()).unwrap();
        let e = &r.entries[0];
        assert_eq!(e.scope, Scope::Both);
        assert!(e.enabled);
        assert!(e.aliases.is_empty());
        assert!(e.note.is_none());
    }

    #[test]
    fn records_line_numbers_for_errors() {
        let t = "Kubernetes\tKubernetes\nBad\t\nReact\tReact";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[1].line_no, 3, "行号应正确记录");
    }
}
