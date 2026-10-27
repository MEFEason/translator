//! YAML 解析（最便于手写的格式：支持注释与分组嵌套）。
//!
//! 支持的子集：
//! - `key: value` 映射
//! - `- ` 列表项
//! - 两空格缩进嵌套
//! - `#` 注释
//! - `groups:` 分组结构
//!
//! 不支持锚点、多文档、折叠标量等高级特性 —— 词库文件不需要。

use super::{finalize, make_entry, ParsedFile};
use crate::error::{Error, Result};
use crate::glossary::schema::Scope;
use std::path::PathBuf;

pub fn parse(text: &str, file: PathBuf) -> Result<ParsedFile> {
    let doc = YamlDoc::parse(text).map_err(|m| Error::GlossaryParse {
        file: file.display().to_string(),
        line: 1,
        msg: m,
    })?;

    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let version = doc
        .scalars
        .get("version")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);

    let mut line_no = 0u32;

    // 结构 A：groups: → name + entries: [ {...} ]
    if !doc.groups.is_empty() {
        for g in &doc.groups {
            let group_name = g.name.clone();
            for item in &g.entries {
                line_no += 1;
                let source = item.scalar("source").unwrap_or_default();
                let target = item.scalar("target").unwrap_or_default();
                let mut e = make_entry(&file, line_no, source, target);
                if let Some(s) = item.scalar("scope").as_deref() {
                    e.scope = Scope::parse(s);
                }
                e.group = Some(group_name.clone());
                if let Some(v) = item.scalar("enabled").as_deref() {
                    e.enabled = super::parse_bool(v, true);
                }
                if let Some(n) = item.scalar("note").as_deref() {
                    if !n.is_empty() {
                        e.note = Some(n.to_string());
                    }
                }
                if let Some(a) = item.scalar("aliases").as_deref() {
                    // 行内数组语法 [a, b] 或逗号分隔
                    e.aliases = parse_inline_list(a);
                }
                entries.push(e);
            }
        }
    }

    // 结构 B：顶层 entries: 列表
    for item in &doc.entries {
        line_no += 1;
        let source = item.scalar("source").unwrap_or_default();
        let target = item.scalar("target").unwrap_or_default();
        let mut e = make_entry(&file, line_no, source, target);
        if let Some(s) = item.scalar("scope").as_deref() {
            e.scope = Scope::parse(s);
        }
        if let Some(g) = item.scalar("group").as_deref() {
            if !g.is_empty() {
                e.group = Some(g.to_string());
            }
        }
        if let Some(v) = item.scalar("enabled").as_deref() {
            e.enabled = super::parse_bool(v, true);
        }
        if let Some(n) = item.scalar("note").as_deref() {
            if !n.is_empty() {
                e.note = Some(n.to_string());
            }
        }
        if let Some(a) = item.scalar("aliases").as_deref() {
            e.aliases = parse_inline_list(a);
        }
        entries.push(e);
    }

    let entries = finalize(entries, &mut warnings);
    Ok(ParsedFile { version, entries })
}

/// 解析 `[a, b]` 或 `a,b`
fn parse_inline_list(s: &str) -> Vec<String> {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']');
    t.split(',')
        .map(|x| x.trim().trim_matches(['"', '\'']).to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// 一个 YAML 映射项（如 `- source: X` 起始的一组键值）
#[derive(Debug, Default)]
struct YamlItem {
    scalars: std::collections::BTreeMap<String, String>,
}

impl YamlItem {
    fn scalar(&self, k: &str) -> Option<String> {
        self.scalars.get(k).cloned()
    }
}

#[derive(Debug, Default)]
struct YamlGroup {
    name: String,
    entries: Vec<YamlItem>,
}

#[derive(Debug, Default)]
struct YamlDoc {
    scalars: std::collections::BTreeMap<String, String>,
    entries: Vec<YamlItem>,
    groups: Vec<YamlGroup>,
}

impl YamlDoc {
    fn parse(text: &str) -> std::result::Result<Self, String> {
        let mut doc = YamlDoc::default();
        let lines: Vec<(usize, String)> = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.to_string()))
            .collect();

        let mut i = 0usize;

        while i < lines.len() {
            let (_, raw) = &lines[i];
            let line = strip_comment(raw);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                i += 1;
                continue;
            }
            let indent = line.len() - line.trim_start().len();

            // 顶层 version: n
            if indent == 0 {
                if let Some(v) = trimmed.strip_prefix("version:") {
                    doc.scalars
                        .insert("version".into(), v.trim().trim_matches(['"', '\'']).to_string());
                    i += 1;
                    continue;
                }
                if trimmed == "groups:" {
                    i = parse_groups(&lines, i + 1, &mut doc)?;
                    continue;
                }
                if trimmed == "entries:" {
                    i = parse_items(&lines, i + 1, 2, &mut doc.entries)?;
                    continue;
                }
            }
            i += 1;
        }

        Ok(doc)
    }
}

/// 解析 groups 段。返回下一个待处理行号。
fn parse_groups(
    lines: &[(usize, String)],
    mut i: usize,
    doc: &mut YamlDoc,
) -> std::result::Result<usize, String> {
    while i < lines.len() {
        let (_, raw) = &lines[i];
        let line = strip_comment(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            return Ok(i); // 回到顶层
        }

        if let Some(rest) = trimmed.strip_prefix("- name:") {
            let group_indent = indent;
            let mut group = YamlGroup {
                name: rest.trim().trim_matches(['"', '\'']).to_string(),
                entries: vec![],
            };
            // 继续读取该组的 entries，遇到同缩进的下一个 "- name:" 即为组边界
            let mut j = i + 1;
            while j < lines.len() {
                let (_, r2) = &lines[j];
                let l2 = strip_comment(r2);
                let t2 = l2.trim();
                if t2.is_empty() {
                    j += 1;
                    continue;
                }
                let ind2 = l2.len() - l2.trim_start().len();
                // 回到顶层，或遇到同层的新分组 → 结束本组
                if ind2 == 0 || (ind2 == group_indent && t2.starts_with("- name:")) {
                    break;
                }
                if t2 == "entries:" {
                    // 列表项缩进 = entries: 的缩进 + 2（标准 YAML 嵌套）
                    j = parse_items(lines, j + 1, ind2 + 2, &mut group.entries)?;
                    continue;
                }
                j += 1;
            }
            doc.groups.push(group);
            i = j;
            continue;
        }
        i += 1;
    }
    Ok(i)
}

/// 解析 `- key: value` 列表项
fn parse_items(
    lines: &[(usize, String)],
    mut i: usize,
    base_indent: usize,
    out: &mut Vec<YamlItem>,
) -> std::result::Result<usize, String> {
    let mut current = YamlItem::default();
    let mut has_content = false;

    while i < lines.len() {
        let (_, raw) = &lines[i];
        let line = strip_comment(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent < base_indent {
            break;
        }

        if let Some(rest) = trimmed.strip_prefix("- ") {
            // 新列表项
            if has_content {
                out.push(std::mem::take(&mut current));
            }
            has_content = false;
            let rest = rest.trim();
            if let Some((k, v)) = rest.split_once(':') {
                let key = k.trim().to_string();
                let val = v.trim().trim_matches(['"', '\'']).to_string();
                if val.is_empty() {
                    has_content = true;
                    current.scalars.insert(key, String::new());
                } else {
                    current.scalars.insert(key, val);
                    has_content = true;
                }
            }
            i += 1;
            continue;
        }

        // 续行 key: value（属于当前项）
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().trim_matches(['"', '\'']).to_string();
            if !val.is_empty() {
                current.scalars.insert(key, val);
                has_content = true;
            } else {
                current.scalars.insert(key, String::new());
            }
        }
        i += 1;
    }

    if has_content {
        out.push(current);
    }
    Ok(i)
}

/// 去掉 `#` 注释（不在引号内的 `#`）
fn strip_comment(line: &str) -> String {
    let bytes: Vec<char> = line.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => {
                // 需在行首或前面是空白才算注释
                if i == 0 || bytes[i - 1].is_whitespace() {
                    return bytes[..i].iter().collect();
                }
            }
            _ => {}
        }
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("test.yaml")
    }

    #[test]
    fn parses_groups_structure() {
        let t = r#"
version: 1
groups:
  - name: 技术术语
    entries:
      - source: Kubernetes
        target: Kubernetes
        note: 官方名称
      - source: React
        target: React
  - name: 产品名
    entries:
      - source: iPhone
        target: iPhone
        scope: both
"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.version, 1);
        assert_eq!(r.entries.len(), 3);
        assert_eq!(r.entries[0].source, "Kubernetes");
        assert_eq!(r.entries[0].group.as_deref(), Some("技术术语"));
        assert_eq!(r.entries[0].note.as_deref(), Some("官方名称"));
        assert_eq!(r.entries[1].source, "React");
        assert_eq!(r.entries[2].group.as_deref(), Some("产品名"));
    }

    #[test]
    fn parses_top_level_entries() {
        let t = r#"
entries:
  - source: A
    target: 甲
  - source: B
    target: 乙
"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[1].target, "乙");
    }

    #[test]
    fn inline_alias_list() {
        let t = r#"
entries:
  - source: Kubernetes
    target: Kubernetes
    aliases: [k8s, K8s]
"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].aliases, vec!["k8s", "K8s"]);
    }

    #[test]
    fn comments_are_stripped() {
        let t = "entries:\n  # 这是注释\n  - source: A  # 行尾注释\n    target: 甲\n";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].source, "A");
        assert_eq!(r.entries[0].target, "甲");
    }

    #[test]
    fn hash_inside_value_is_preserved() {
        let t = "entries:\n  - source: \"C#\"\n    target: \"C井\"\n";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].source, "C#");
    }

    #[test]
    fn scope_and_enabled_parsed() {
        let t = "entries:\n  - source: A\n    target: 甲\n    scope: mt\n    enabled: false\n";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].scope, Scope::Mt);
        assert!(!r.entries[0].enabled);
    }

    #[test]
    fn quoted_values_strip_quotes() {
        let t = "entries:\n  - source: \"A\"\n    target: '甲'\n";
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].source, "A");
        assert_eq!(r.entries[0].target, "甲");
    }
}
