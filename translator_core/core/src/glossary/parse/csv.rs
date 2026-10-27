//! CSV 解析（RFC 4180：引号转义 + 字段内逗号）。

use super::{finalize, is_skippable, make_entry, parse_bool, parse_multi, ParsedFile};
use crate::error::Result;
use crate::glossary::schema::Scope;
use std::path::PathBuf;

pub fn parse(text: &str, file: PathBuf) -> Result<ParsedFile> {
    let records = split_records(text);
    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let mut columns: Option<Vec<usize>> = None;
    let mut line_no = 0u32;

    for cells in records {
        line_no += 1;
        if cells.is_empty() || cells.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let first = cells[0].trim();
        if first.starts_with('#') {
            continue;
        }

        let trimmed: Vec<&str> = cells.iter().map(|s| s.trim()).collect();

        if columns.is_none() {
            let lower: Vec<String> = trimmed.iter().map(|c| c.to_ascii_lowercase()).collect();
            if lower.iter().any(|c| c == "source") && lower.iter().any(|c| c == "target") {
                let mut cols = vec![usize::MAX; 7];
                let names = ["source", "target", "aliases", "scope", "group", "enabled", "note"];
                for (ci, name) in lower.iter().enumerate() {
                    if let Some(hi) = names.iter().position(|h| h == name) {
                        cols[hi] = ci;
                    }
                }
                columns = Some(cols);
                continue;
            }
            columns = Some(vec![0usize, 1, 2, 3, 4, 5, 6]);
        }

        let cols = columns.as_ref().unwrap();
        let get = |i: usize| -> &str {
            cols.get(i)
                .filter(|&&c| c != usize::MAX)
                .and_then(|&c| trimmed.get(c))
                .copied()
                .unwrap_or("")
        };

        // CSV 内 aliases 用 `;` 分隔（避免与字段内逗号冲突）
        let mut e = make_entry(&file, line_no, get(0).to_string(), get(1).to_string());
        e.aliases = parse_multi(get(2), ';');
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
    Ok(ParsedFile { version: 1, entries })
}

/// RFC 4180 记录切分，处理引号转义（`""` 表示字面量 `"`）与字段内换行。
pub fn split_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next(); // 转义的双引号
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.trim().is_empty() => in_quotes = true,
            ',' => {
                current.push(std::mem::take(&mut field));
            }
            '\r' => { /* 忽略，等 \n */ }
            '\n' => {
                current.push(std::mem::take(&mut field));
                if !(current.len() == 1 && current[0].trim().is_empty()) {
                    records.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !current.is_empty() {
        current.push(field);
        if !(current.len() == 1 && current[0].trim().is_empty()) {
            records.push(current);
        }
    }
    records
}

/// 空白判定辅助（供 mod.rs 复用）
#[allow(dead_code)]
fn is_blank(s: &str) -> bool {
    is_skippable(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("test.csv")
    }

    #[test]
    fn simple_records() {
        let t = "Kubernetes,Kubernetes\ngRPC,gRPC";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].source, "Kubernetes");
    }

    #[test]
    fn quoted_field_with_comma() {
        let t = "\"Kubernetes\",\"官方名称, 不翻译\"";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].target, "官方名称, 不翻译");
    }

    #[test]
    fn escaped_double_quote() {
        let t = "\"say \"\"hi\"\"\",\"说\"\"你好\"\"\"";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].source, "say \"hi\"");
        assert_eq!(r.entries[0].target, "说\"你好\"");
    }

    #[test]
    fn aliases_use_semicolon_in_csv() {
        let t = "Kubernetes,Kubernetes,\"k8s;K8s\",both,技术";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].aliases, vec!["k8s", "K8s"]);
        assert_eq!(r.entries[0].group.as_deref(), Some("技术"));
    }

    #[test]
    fn header_mapping() {
        let t = "source,target,enabled\nKubernetes,Kubernetes,false\nReact,React,true";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert!(!r.entries[0].enabled);
        assert!(r.entries[1].enabled);
    }

    #[test]
    fn record_split_handles_multiline_field() {
        let recs = split_records("a,\"line1\nline2\",c\n");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].len(), 3);
        assert_eq!(recs[0][1], "line1\nline2");
    }

    #[test]
    fn empty_lines_ignored() {
        let recs = split_records("a,b\n\n\nc,d\n");
        assert_eq!(recs.len(), 2);
    }

    #[test]
    fn crlf_handled() {
        let t = "Kubernetes,Kubernetes\r\nReact,React\r\n";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
    }
}
