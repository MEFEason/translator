//! JSON 解析。
//!
//! 不引入 serde_json：本 crate 的依赖策略是「核心零重依赖」，
//! 词库解析属可选路径，此处实现最小 JSON 子集解析器
//! （对象 / 数组 / 字符串 / 数字 / 布尔 / null）。

use super::{finalize, make_entry, ParsedFile};
use crate::error::{Error, Result};
use crate::glossary::schema::Scope;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// JSON 值（仅解析词库需要的类型）
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    /// 取整数。
    ///
    /// 兼容**数字字符串** —— 某些 JSON 生成器会把所有标量都写成字符串
    /// （`{"asr_threads":"6"}`）。配置解析不能因此丢字段，
    /// 否则 UI 传的线程数会被静默忽略。
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(n) => Some(*n as i64),
            Json::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        }
    }
}

pub fn parse(text: &str, file: PathBuf) -> Result<ParsedFile> {
    parse_glossary(text, &file)
}

/// 仅解析 JSON 值（供 MT 回退分词器复用 vocab.json）
pub fn parse_value(text: &str) -> Result<Json> {
    let file = PathBuf::from("<memory>");
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
        line: 1,
        file: file.clone(),
    };
    parser.skip_ws();
    let root = parser.value()?;
    parser.skip_ws();
    if parser.pos < parser.bytes.len() {
        return Err(Error::GlossaryParse {
            file: file.display().to_string(),
            line: parser.line,
            msg: "JSON 尾部存在多余内容".to_string(),
        });
    }
    Ok(root)
}

/// 解析词库文件主体
fn parse_glossary(text: &str, file: &PathBuf) -> Result<ParsedFile> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
        line: 1,
        file: file.clone(),
    };
    parser.skip_ws();
    let root = parser.value()?;
    parser.skip_ws();

    if parser.pos < parser.bytes.len() {
        return Err(Error::GlossaryParse {
            file: file.display().to_string(),
            line: parser.line,
            msg: "JSON 尾部存在多余内容".to_string(),
        });
    }

    let version = root
        .get("version")
        .and_then(|v| match v {
            Json::Number(n) => Some(*n as u32),
            _ => None,
        })
        .unwrap_or(1);

    let mut warnings = Vec::new();
    let mut entries = Vec::new();

    let arr = root
        .get("entries")
        .and_then(|e| e.as_array())
        .ok_or_else(|| Error::GlossaryParse {
            file: file.display().to_string(),
            line: 1,
            msg: "缺少 entries 数组".to_string(),
        })?;

    for (i, item) in arr.iter().enumerate() {
        let source = item.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let target = item.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let mut e = make_entry(&file, (i + 1) as u32, source.to_string(), target.to_string());

        if let Some(a) = item.get("aliases").and_then(|v| v.as_array()) {
            e.aliases = a
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect();
        }
        if let Some(s) = item.get("scope").and_then(|v| v.as_str()) {
            e.scope = Scope::parse(s);
        }
        if let Some(c) = item.get("case_sensitive").and_then(|v| v.as_bool()) {
            e.case_sensitive = c;
        }
        if let Some(g) = item.get("group").and_then(|v| v.as_str()) {
            if !g.is_empty() {
                e.group = Some(g.to_string());
            }
        }
        if let Some(en) = item.get("enabled").and_then(|v| v.as_bool()) {
            e.enabled = en;
        }
        if let Some(n) = item.get("note").and_then(|v| v.as_str()) {
            if !n.is_empty() {
                e.note = Some(n.to_string());
            }
        }
        if let Some(r) = item.get("regex").and_then(|v| v.as_str()) {
            if !r.is_empty() {
                e.regex = Some(r.to_string());
            }
        }
        entries.push(e);
    }

    let entries = finalize(entries, &mut warnings);
    Ok(ParsedFile { version, entries })
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    file: PathBuf,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> Error {
        Error::GlossaryParse {
            file: self.file.display().to_string(),
            line: self.line,
            msg: msg.to_string(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b'\n' {
                self.line += 1;
                self.pos += 1;
            } else if c.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn value(&mut self) -> Result<Json> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(_) => self.number(),
            None => Err(self.err("意外到达文件末尾")),
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Result<Json> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err(&format!("期望 {word}")))
        }
    }

    fn object(&mut self) -> Result<Json> {
        self.pos += 1; // {
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("期望 ':'"));
            }
            self.pos += 1;
            let v = self.value()?;
            map.insert(key, v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.err("对象中期望 ',' 或 '}'")),
            }
        }
    }

    fn array(&mut self) -> Result<Json> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            let v = self.value()?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.err("数组中期望 ',' 或 ']'")),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        if self.peek() != Some(b'"') {
            return Err(self.err("期望字符串"));
        }
        self.pos += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek().ok_or_else(|| self.err("转义未完成"))?;
                    self.pos += 1;
                    match esc {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'u' => {
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                                .map_err(|_| self.err("\\u 转义非法"))?;
                            let code =
                                u32::from_str_radix(hex, 16).map_err(|_| self.err("\\u 非法"))?;
                            self.pos += 4;
                            // 处理代理对
                            if (0xD800..0xDC00).contains(&code)
                                && self.bytes.get(self.pos) == Some(&b'\\')
                                && self.bytes.get(self.pos + 1) == Some(&b'u')
                            {
                                let hex2 =
                                    std::str::from_utf8(&self.bytes[self.pos + 2..self.pos + 6])
                                        .map_err(|_| self.err("\\u 代理对非法"))?;
                                if let Ok(low) = u32::from_str_radix(hex2, 16) {
                                    if (0xDC00..0xE000).contains(&low) {
                                        self.pos += 6;
                                        let combined =
                                            0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                        if let Some(ch) = char::from_u32(combined) {
                                            out.push(ch);
                                            continue;
                                        }
                                    }
                                }
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.err("未知转义序列")),
                    }
                }
                b'\n' => return Err(self.err("字符串未闭合")),
                _ => {
                    // 复制一个完整 UTF-8 字符
                    let s = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| self.err("非法 UTF-8"))?;
                    let ch = s.chars().next().ok_or_else(|| self.err("字符串未闭合"))?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Err(self.err("字符串未闭合"))
    }

    fn number(&mut self) -> Result<Json> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' || c == b'e' || c == b'E'
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("数字非法"))?;
        s.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| self.err("数字非法"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("test.json")
    }

    #[test]
    fn parses_full_entry() {
        let t = r#"{
          "version": 1,
          "entries": [
            {
              "source": "Kubernetes",
              "target": "Kubernetes",
              "aliases": ["k8s", "K8s"],
              "scope": "both",
              "case_sensitive": false,
              "group": "技术",
              "enabled": true,
              "note": "官方名称"
            }
          ]
        }"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries.len(), 1);
        let e = &r.entries[0];
        assert_eq!(e.source, "Kubernetes");
        assert_eq!(e.aliases, vec!["k8s", "K8s"]);
        assert_eq!(e.group.as_deref(), Some("技术"));
        assert_eq!(e.note.as_deref(), Some("官方名称"));
    }

    #[test]
    fn minimal_entry_uses_defaults() {
        let t = r#"{"entries":[{"source":"A","target":"甲"}]}"#;
        let r = parse(t, p()).unwrap();
        let e = &r.entries[0];
        assert_eq!(e.scope, Scope::Both);
        assert!(e.enabled);
        assert!(!e.case_sensitive);
        assert!(e.aliases.is_empty());
    }

    #[test]
    fn unicode_escape_handled() {
        let t = r#"{"entries":[{"source":"A","target":"\u4E2D\u6587"}]}"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].target, "中文");
    }

    #[test]
    fn surrogate_pair_handled() {
        let t = r#"{"entries":[{"source":"A","target":"\uD83D\uDE00"}]}"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].target, "😀");
    }

    #[test]
    fn literal_chinese_passthrough() {
        let t = r#"{"entries":[{"source":"i.e.","target":"也就是说"}]}"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].target, "也就是说");
    }

    #[test]
    fn missing_entries_is_error() {
        assert!(parse(r#"{"version":1}"#, p()).is_err());
    }

    #[test]
    fn malformed_json_reports_line() {
        let t = "{\n\"entries\": [\n{\"source\": }\n]}";
        let err = parse(t, p()).unwrap_err();
        assert!(matches!(err, Error::GlossaryParse { .. }));
    }

    #[test]
    fn trailing_garbage_is_error() {
        let t = r#"{"entries":[]} trailing"#;
        assert!(parse(t, p()).is_err());
    }

    #[test]
    fn escaped_backslash_and_quote() {
        let t = r#"{"entries":[{"source":"a\\b","target":"c\"d"}]}"#;
        let r = parse(t, p()).unwrap();
        assert_eq!(r.entries[0].source, "a\\b");
        assert_eq!(r.entries[0].target, "c\"d");
    }
}
