//! 转写盘读取：按 seq 范围取行，并把译文 patch 合并进去。
//!
//! ## 为什么整个文件读进内存
//!
//! 1 小时会话约 200KB，8 小时约 1.6MB —— 直接 `read_to_end` 比写一个
//! 带缓冲的流式解析器简单得多，而且快得多（一次系统调用 vs 几千次）。
//! 真正需要流式的是「边写边读」的场景，本项目不在实时路径上读盘。
//!
//! ## 断电截断的处理
//!
//! 扫描遇到截断记录就**停下并保留已解出的部分**，绝不 panic、绝不跳过继续
//! （跳过会导致 payload_len 错位，把后面的记录全解析成乱码）。

use std::collections::HashMap;
use std::path::Path;

use super::record::{decode, unescape, Decoded, Kind, Record};

/// 从转写盘还原出来的一行（已合并译文）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub seq: u64,
    pub source: String,
    /// 说话人标签（旧文件读出为空）
    pub speaker: String,
    pub target: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub needs_review: bool,
    pub forced_cut: bool,
}

/// 读取结果
#[derive(Debug, Clone, Default)]
pub struct Replay {
    pub lines: Vec<TranscriptLine>,
    /// 因断电截断而丢失的尾部记录数。>0 说明上次会话异常终止。
    pub truncated_tail: bool,
}

/// 全量回放：合并所有记录成最终行
pub fn replay(path: &Path) -> std::io::Result<Replay> {
    let buf = std::fs::read(path)?;
    Ok(scan(&buf, None, usize::MAX))
}

/// 取 `from_seq` 之后的最多 `max_lines` 行（按 seq 升序）
///
/// `from_seq = None` 表示从头开始。
pub fn query(path: &Path, from_seq: Option<u64>, max_lines: usize) -> std::io::Result<Replay> {
    let buf = std::fs::read(path)?;
    Ok(scan(&buf, from_seq, max_lines))
}

/// 扫描并合并
pub fn scan(buf: &[u8], from_seq: Option<u64>, max_lines: usize) -> Replay {
    let mut lines: Vec<TranscriptLine> = Vec::new();
    let mut index: HashMap<u64, usize> = HashMap::new();
    let mut truncated = false;
    let mut pos = 0usize;

    while let Some((dec, next)) = decode(buf, pos) {
        match dec {
            Decoded::Truncated => {
                truncated = true;
                break;
            }
            Decoded::Ok(rec) => {
                pos = next;
                apply(&rec, &mut lines, &mut index, from_seq, max_lines);
            }
        }
    }

    lines.sort_by_key(|l| l.seq);
    Replay {
        lines,
        truncated_tail: truncated,
    }
}

fn apply(
    rec: &Record,
    lines: &mut Vec<TranscriptLine>,
    index: &mut HashMap<u64, usize>,
    from_seq: Option<u64>,
    max_lines: usize,
) {
    match rec.kind {
        Kind::Add => {
            // from_seq 之前的一律不展开 —— 展开它们没有意义，
            // 但**必须在 index 里登记**，否则后面的 fill 找不到落点。
            if let Some(from) = from_seq {
                if rec.seq < from {
                    return;
                }
            }
            if index.contains_key(&rec.seq) {
                return; // 重复的 add，保留先到的
            }
            // 窗口已满：后面的行不展开。**不登记 index**，
            // 这样它后续的 fill 补丁也找不到落点，一并丢弃 —— 保持一致。
            if lines.len() >= max_lines {
                return;
            }
            index.insert(rec.seq, lines.len());
            let (speaker, source) = rec.add_parts();
            lines.push(TranscriptLine {
                seq: rec.seq,
                source,
                speaker,
                target: None,
                start_ms: rec.start_ms,
                end_ms: rec.end_ms,
                needs_review: rec.needs_review(),
                forced_cut: rec.forced_cut(),
            });
        }
        Kind::Fill => {
            // 译文可能落在 from_seq 之前的行上 —— 那种情况行不在列表里，
            // 直接忽略（调用方已经拿到那行的旧版本，或它已滚出屏幕）。
            let Some(&i) = index.get(&rec.seq) else {
                return;
            };
            let t = rec.fill_target();
            // 空译文 = 翻译失败。不要用空串覆盖已有译文。
            if t.is_empty() {
                lines[i].needs_review = true;
            } else {
                lines[i].target = Some(t);
            }
            if rec.needs_review() {
                lines[i].needs_review = true;
            }
        }
        Kind::Rev => {
            if let Some(&i) = index.get(&rec.seq) {
                lines[i].needs_review = true;
            }
        }
    }
}

/// 把行集序列化成 JSON 数组（供 FFI `translator_transcript_query` 返回给 Dart）
pub fn to_json(replay: &Replay) -> String {
    let mut s = String::from("{\"lines\":[");
    for (i, l) in replay.lines.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"seq\":{},\"src\":{},\"tgt\":{},\"st\":{},\"et\":{},\"rev\":{},\"cut\":{}}}",
            l.seq,
            super::record::json_escape(&l.source),
            match &l.target {
                Some(t) => super::record::json_escape(t),
                None => "null".to_string(),
            },
            l.start_ms,
            l.end_ms,
            l.needs_review,
            l.forced_cut
        ));
    }
    s.push_str(&format!(
        "],\"truncated\":{}}}",
        replay.truncated_tail
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::record::Record;

    fn enc(records: &[Record]) -> Vec<u8> {
        let mut buf = Vec::new();
        for r in records {
            buf.extend(r.encode());
        }
        buf
    }

    #[test]
    fn add_then_fill_merges() {
        let buf = enc(&[
            Record::add(0, 0, 100, "we deploy Kubernetes", false, "对方"),
            Record::fill(0, "我们部署 Kubernetes", false),
        ]);
        let r = scan(&buf, None, 100);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].source, "we deploy Kubernetes");
        assert_eq!(r.lines[0].target.as_deref(), Some("我们部署 Kubernetes"));
        assert!(!r.truncated_tail);
    }

    #[test]
    fn untranslated_line_stays_none() {
        let buf = enc(&[Record::add(0, 0, 100, "hello", false, "对方")]);
        let r = scan(&buf, None, 100);
        assert_eq!(r.lines[0].target, None);
    }

    #[test]
    fn lines_are_sorted_by_seq() {
        // 故意乱序写入，读出来必须按 seq 排好
        let buf = enc(&[
            Record::add(2, 200, 300, "third", false, "对方"),
            Record::add(0, 0, 100, "first", false, "对方"),
            Record::add(1, 100, 200, "second", false, "对方"),
        ]);
        let r = scan(&buf, None, 100);
        let seqs: Vec<u64> = r.lines.iter().map(|l| l.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[test]
    fn query_from_seq_skips_earlier_lines() {
        let mut recs: Vec<Record> = (0..10)
            .map(|i| Record::add(i, i * 100, i * 100 + 50, &format!("line{i}"), false, "对方"))
            .collect();
        // 给前几条补上译文
        for i in 0..5 {
            recs.push(Record::fill(i, &format!("译{i}"), false));
        }
        let buf = enc(&recs);
        let r = scan(&buf, Some(7), 100);
        assert_eq!(r.lines.len(), 3, "seq 7/8/9");
        assert_eq!(r.lines[0].seq, 7);
    }

    #[test]
    fn query_respects_max_lines() {
        let recs: Vec<Record> = (0..20)
            .map(|i| Record::add(i, i * 10, i * 10 + 5, &format!("l{i}"), false, "对方"))
            .collect();
        let buf = enc(&recs);
        let r = scan(&buf, None, 5);
        assert_eq!(r.lines.len(), 5);
        assert_eq!(r.lines[0].seq, 0);
    }

    #[test]
    fn truncated_tail_keeps_earlier_lines() {
        // 场景一：第二条记录写到一半断电
        let mut buf = enc(&[
            Record::add(0, 0, 100, "good one", false, "对方"),
            Record::add(1, 100, 200, "good two", false, "我"),
        ]);
        buf.truncate(buf.len() - 5);
        let r = scan(&buf, None, 100);
        assert!(r.truncated_tail, "应标记出截断");
        assert_eq!(r.lines.len(), 1, "完好的第一条必须保留");
        assert_eq!(r.lines[0].source, "good one");
    }

    #[test]
    fn missing_tail_records_are_not_reported_as_truncated() {
        // 场景二：文件正好在记录边界结束（正常收尾，不是断电）
        let rec0 = Record::add(0, 0, 100, "good one", false, "对方");
        let rec1 = Record::add(1, 100, 200, "good two", false, "我");
        let mut buf = rec0.encode();
        buf.extend(rec1.encode());
        buf.truncate(rec0.encode().len()); // 只留第一条

        let r = scan(&buf, None, 100);
        assert_eq!(r.lines.len(), 1);
        assert!(
            !r.truncated_tail,
            "边界处结束不算截断 —— 否则每次正常会话都会被误报"
        );
    }

    #[test]
    fn empty_fill_marks_review_and_keeps_old_target() {
        let buf = enc(&[
            Record::add(0, 0, 100, "hello", false, "对方"),
            Record::fill(0, "你好", false),
            Record::fill(0, "", true), // 翻译失败的补丁
        ]);
        let r = scan(&buf, None, 100);
        assert_eq!(r.lines[0].target.as_deref(), Some("你好"), "空译文不得覆盖已有译文");
        assert!(r.lines[0].needs_review);
    }

    #[test]
    fn fill_without_add_is_ignored() {
        let buf = enc(&[Record::fill(99, "孤儿译文", false)]);
        let r = scan(&buf, None, 100);
        assert!(r.lines.is_empty(), "没有 add 的 fill 不该凭空造行");
    }

    #[test]
    fn duplicate_add_keeps_first() {
        let buf = enc(&[
            Record::add(0, 0, 100, "first", false, "对方"),
            Record::add(0, 0, 100, "second", false, "对方"),
        ]);
        let r = scan(&buf, None, 100);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].source, "first");
    }

    #[test]
    fn json_output_is_parseable_shape() {
        let buf = enc(&[
            Record::add(0, 0, 100, "hi", false, "对方"),
            Record::fill(0, "你好", true),
        ]);
        let j = to_json(&scan(&buf, None, 100));
        assert!(j.starts_with("{\"lines\":[{"));
        assert!(j.contains("\"src\":\"hi\""));
        assert!(j.contains("\"tgt\":\"你好\""));
        assert!(j.contains("\"rev\":true"));
        assert!(j.ends_with("\"truncated\":false}"), "{j}");
    }

    #[test]
    fn json_escapes_quotes_in_text() {
        let buf = enc(&[Record::add(0, 0, 1, "he said \"hi\"", false, "对方")]);
        let j = to_json(&scan(&buf, None, 100));
        assert!(j.contains("\\\""), "引号必须转义，否则 JSON 非法: {j}");
    }

    #[test]
    fn forced_cut_flag_survives() {
        let buf = enc(&[Record::add(0, 0, 100, "x", true, "对方")]);
        assert!(scan(&buf, None, 10).lines[0].forced_cut);
    }
}