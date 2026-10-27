//! 转写盘：实时落盘的会话记录。
//!
//! ## 它解决什么问题
//!
//! 内存里只能放最近几千行（8 小时会议会到 10000 行以上，再多就占内存了）。
//! 但用户可能想回看整场会的字幕。所以：
//!
//! - **实时写入**：一行 EN 就位立刻落盘，不等会话结束
//! - **有上限**：内存窗口超限时丢弃最旧的
//! - **可回溯**：更早的内容从文件按 seq 范围读回来
//!
//! ## 格式与取舍
//!
//! 见 [`record`] 模块的说明。核心是 **append-only + 定长头**：
//! 断电最多丢末尾一条，且能看出丢了；不像原地改文件那样可能写坏中间。
//!
//! ## 降级策略
//!
//! 写盘失败**只降级为仅内存**，绝不中断转写（详见 [`writer`]）。
//! 但降级必须可见 —— 错误会通过 [`crate::pipeline::TranscriptSink::last_error`]
//! 一路传到 UI。

pub mod reader;
pub mod record;
pub mod writer;

pub use reader::{query, replay, Replay, TranscriptLine};

/// 把转写盘读出的行格式化为 SRT / VTT / TXT。
///
/// `format`：0=SRT 1=VTT 2=TXT。
/// TXT 是**给人读的对话记录**（时间戳 + 说话人 + 原文 + 译文），
/// SRT/VTT 是给播放器与剪辑软件用的字幕格式。
pub fn format_replay(lines: &[crate::transcript::TranscriptLine], format: i32) -> String {
    match format {
        0 => {
            let mut out = String::new();
            for (i, l) in lines.iter().enumerate() {
                out.push_str(&format!("{}\n", i + 1));
                out.push_str(&format!(
                    "{} --> {}\n",
                    srt_stamp(l.start_ms),
                    srt_stamp(l.end_ms)
                ));
                out.push_str(&l.source);
                out.push('\n');
                if let Some(t) = &l.target {
                    out.push_str(t);
                    out.push('\n');
                }
                out.push('\n');
            }
            out
        }
        1 => {
            let mut out = String::from("WEBVTT\n\n");
            for l in lines {
                out.push_str(&format!(
                    "{} --> {}\n",
                    srt_stamp(l.start_ms).replace(',', "."),
                    srt_stamp(l.end_ms).replace(',', ".")
                ));
                out.push_str(&l.source);
                out.push('\n');
                if let Some(t) = &l.target {
                    out.push_str(t);
                    out.push('\n');
                }
                out.push('\n');
            }
            out
        }
        _ => {
            // TXT：给人读的对话记录
            let mut out = String::new();
            for l in lines {
                let spk = if l.speaker.is_empty() { "对方" } else { &l.speaker };
                out.push_str(&format!("[{}] {}\n", hhmmss(l.start_ms), spk));
                out.push_str(&format!("  EN: {}\n", l.source));
                match &l.target {
                    Some(t) => out.push_str(&format!("  ZH: {t}\n")),
                    None => out.push_str("  ZH: （翻译中）\n"),
                }
                if l.needs_review {
                    out.push_str("  ⚠ 术语待复核\n");
                }
                out.push('\n');
            }
            out
        }
    }
}

/// ms → `HH:MM:SS,mmm`（SRT 时间戳）
pub fn srt_stamp(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let mss = ms % 1000;
    format!("{h:02}:{m:02}:{s:02},{mss:03}")
}

/// ms → `HH:MM:SS`
pub fn hhmmss(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    format!("{h:02}:{m:02}:{s:02}")
}

pub use record::{json_escape, Kind, Record};
pub use writer::{list_sessions, session_file_name, TranscriptWriter};

/// 内存窗口丢弃旧行后，用户仍能从转写盘取回的最小保证。
///
/// 这是 UI 的诚实性承诺：告诉用户「更早的内容在文件里」之前，
/// 必须真的能取到。
pub fn history_available_before(dir: &std::path::Path, earliest_seq_in_memory: u64) -> bool {
    list_sessions(dir)
        .ok()
        .and_then(|v| v.into_iter().next())
        .and_then(|f| replay(&f).ok())
        .map(|r| r.lines.iter().any(|l| l.seq < earliest_seq_in_memory))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::TranscriptSink; // append_add / append_fill 是 trait 方法
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("translator-tr-{tag}-{uniq}"));
        Tmp(p)
    }

    #[test]
    fn writer_then_replay_roundtrips() {
        let t = tmp("rt");
        let mut w = TranscriptWriter::create_in(&t.0).unwrap();
        w.append_add(0, "we deploy Kubernetes", 0, 1000, false, "对方");
        w.append_fill(0, "我们部署 Kubernetes", false);
        w.append_add(1, "second sentence", 1000, 2000, true, "我");
        w.finish();

        let f = list_sessions(&t.0).unwrap().pop().unwrap();
        let r = replay(&f).unwrap();
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].source, "we deploy Kubernetes");
        assert_eq!(r.lines[0].target.as_deref(), Some("我们部署 Kubernetes"));
        assert_eq!(r.lines[1].target, None, "第二行没译文");
        assert!(r.lines[1].forced_cut);
    }

    #[test]
    fn history_is_available_after_memory_drops_oldest() {
        // 这是 N3+N4 的核心保证：内存丢了，文件还在
        let t = tmp("hist");
        let mut w = TranscriptWriter::create_in(&t.0).unwrap();
        for i in 0..100u64 {
            w.append_add(i, &format!("line{i}"), i * 100, i * 100 + 50, false, "对方");
        }
        w.finish();

        let f = list_sessions(&t.0).unwrap().pop().unwrap();
        let r = replay(&f).unwrap();
        assert_eq!(r.lines.len(), 100);

        // 内存只剩 seq >= 80
        assert!(
            r.lines.iter().any(|l| l.seq < 80),
            "内存窗口外的行必须能从文件取回"
        );
        assert!(history_available_before(&t.0, 80));
    }

    #[test]
    fn history_available_false_when_empty() {
        let t = tmp("empty");
        std::fs::create_dir_all(&t.0).unwrap();
        assert!(!history_available_before(&t.0, 10));
    }

    #[test]
    fn query_pages_backwards_through_history() {
        let t = tmp("page");
        let mut w = TranscriptWriter::create_in(&t.0).unwrap();
        for i in 0..50u64 {
            w.append_add(i, &format!("line{i}"), i * 10, i * 10 + 5, false, "对方");
            w.append_fill(i, &format!("译{i}"), false);
        }
        w.finish();
        let f = list_sessions(&t.0).unwrap().pop().unwrap();

        // 模拟 UI 向上滚动：分两页取
        let newest = query(&f, Some(30), 100).unwrap();
        assert_eq!(newest.lines.len(), 20);
        let older = query(&f, None, 30).unwrap();
        assert_eq!(older.lines.len(), 30);
        assert_eq!(older.lines[0].seq, 0);
        // 两页无重叠
        assert!(older.lines.iter().all(|l| l.seq < 30));
    }
}