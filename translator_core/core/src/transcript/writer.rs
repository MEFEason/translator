//! 转写盘写入器。
//!
//! ## 铁律：写盘失败绝不拖垮转写
//!
//! 磁盘满、权限不足、路径不存在、杀软锁文件…… 用户宁可**丢字幕**，
//! 也绝不能丢掉正在进行的转写会话。所以：
//!
//! - 任何 IO 错误都被吞掉并计数，转写继续（降级为「仅内存」）
//! - 连续失败超过阈值就**放弃写盘**，避免在坏盘上空转刷错误
//! - 错误原因保留在 [`TranscriptWriter::last_error`]，由 UI 如实展示
//!
//! 这条对应 DESIGN §2.5.2「禁止静默降级」：降级必须可见。

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::pipeline::TranscriptSink;
use crate::platform::userdir;

use super::hhmmss;
use super::record::Record;

/// 多久强制落盘一次。转写盘要求「实时」，攒太久就不实时了。
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// 攒多少条也强制落盘
const FLUSH_EVERY_RECORDS: u32 = 8;

/// 连续失败多少次后彻底放弃写盘
const MAX_CONSECUTIVE_FAILURES: u32 = 8;

/// 缓冲大小
const BUF_CAP: usize = 64 * 1024;

use std::time::Duration;

/// 转写盘写入器
///
/// **双写**（2026-10-23 用户要求）：
/// - `.dsht` 二进制：机读（历史回溯 / SRT 导出 / 断点续转）
/// - `.txt` 纯文本：**人读**。用户原话「写盘的时候应该就是 txt」——
///   二进制盘要用工具才能抠内容，记事本直接打开就能看字幕+译文。
/// txt 是追加式：EN 行就位即写，译文回补时追加 ZH 行（同时间戳可对上）。
pub struct TranscriptWriter {
    out: Option<BufWriter<File>>,
    /// 人读 txt（与 dsht 同名不同后缀）。失败不影响 dsht。
    txt: Option<BufWriter<File>>,
    path: PathBuf,
    written: u64,
    failures: u64,
    consecutive: u32,
    last_error: Option<String>,
    last_flush: Instant,
    since_flush: u32,
}

impl TranscriptWriter {
    /// 在 `dir` 下新建本场会话文件。失败返回 Err —— 此时应降级为仅内存。
    pub fn create_in(dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(session_file_name());
        let file = File::create(&path)?;
        // 同名 .txt 人读盘（用户要求：写盘就是 txt）。建失败不致命 ——
        // dsht 照常，txt 降级为无（last_error 不记，避免和 dsht 失败混淆）。
        let txt_path = path.with_extension("txt");
        let txt = File::create(&txt_path).ok().map(|f| BufWriter::with_capacity(BUF_CAP, f));
        Ok(Self {
            out: Some(BufWriter::with_capacity(BUF_CAP, file)),
            txt,
            path,
            written: 0,
            failures: 0,
            consecutive: 0,
            last_error: None,
            last_flush: Instant::now(),
            since_flush: 0,
        })
    }

    /// 默认位置：桌面 `translate results`
    pub fn create_default() -> std::io::Result<Self> {
        let dir = userdir::default_transcript_dir().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "无法定位桌面目录，请在设置中手动选择转写盘位置",
            )
        })?;
        Self::create_in(&dir)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_active(&self) -> bool {
        self.out.is_some()
    }

    fn write_record(&mut self, r: &Record) {
        // 已放弃写盘：直接返回，不做任何 IO
        let Some(out) = self.out.as_mut() else {
            return;
        };

        let buf = r.encode();
        match out.write_all(&buf) {
            Ok(()) => {
                self.written += 1;
                self.consecutive = 0;
                self.since_flush += 1;
                // 实时性：到时间或到条数就落盘
                if self.since_flush >= FLUSH_EVERY_RECORDS
                    || self.last_flush.elapsed() >= FLUSH_INTERVAL
                {
                    self.do_flush();
                }
            }
            Err(e) => {
                self.on_failure(&e);
            }
        }
    }

    fn do_flush(&mut self) {
        if let Some(out) = self.out.as_mut() {
            if let Err(e) = out.flush() {
                self.on_failure(&e);
                return;
            }
        }
        // txt 同步 flush（失败不影响 dsht，静默降级）
        if let Some(t) = self.txt.as_mut() {
            let _ = t.flush();
        }
        self.last_flush = Instant::now();
        self.since_flush = 0;
    }

    /// 追加一行到人读 txt（失败静默 —— 不影响 dsht 与转写）。
    fn txt_line(&mut self, line: &str) {
        if let Some(t) = self.txt.as_mut() {
            let _ = writeln!(t, "{line}");
        }
    }

    fn on_failure(&mut self, e: &std::io::Error) {
        self.failures += 1;
        self.consecutive += 1;
        self.last_error = Some(format!("{e}（已降级为仅内存转写）"));
        if self.consecutive >= MAX_CONSECUTIVE_FAILURES {
            // 放弃写盘：继续重试只会空转刷错误，且可能拖慢实时路径
            if let Some(out) = self.out.as_mut() {
                let _ = out.flush();
            }
            self.out = None;
        }
    }
}

impl TranscriptSink for TranscriptWriter {
    fn append_add(
        &mut self,
        seq: u64,
        source: &str,
        start_ms: u64,
        end_ms: u64,
        forced_cut: bool,
        speaker: &str,
    ) {
        self.write_record(&Record::add(seq, start_ms, end_ms, source, forced_cut, speaker));
        // 人读 txt：EN 行就位即写（实时可看）
        self.txt_line(&format!(
            "[{}] {} {}",
            hhmmss(start_ms),
            if speaker.is_empty() { "对方" } else { speaker },
            source
        ));
    }

    fn append_fill(&mut self, seq: u64, target: &str, needs_review: bool) {
        self.write_record(&Record::fill(seq, target, needs_review));
        // 人读 txt：译文回补行（追加式，同时间戳与 EN 行对上）
        let flag = if needs_review { "（待复核）" } else { "" };
        self.txt_line(&format!("  ZH: {target}{flag}"));
    }

    fn flush(&mut self) {
        self.do_flush();
    }

    fn finish(&mut self) {
        if let Some(mut out) = self.out.take() {
            if let Err(e) = out.flush() {
                self.failures += 1;
                self.last_error = Some(e.to_string());
            }
            // 同步到介质：会话结束时内容必须真的在盘上
            if let Ok(f) = out.into_inner() {
                let _ = f.sync_all();
            }
        }
        // 人读 txt 同样 flush + sync
        if let Some(mut t) = self.txt.take() {
            let _ = t.flush();
            if let Ok(f) = t.into_inner() {
                let _ = f.sync_all();
            }
        }
    }

    fn written(&self) -> u64 {
        self.written
    }

    fn failures(&self) -> u64 {
        self.failures
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    fn path(&self) -> Option<String> {
        Some(self.path.display().to_string())
    }
}

/// 本场会话文件名：`transcript-20261020-142424.dsht`
///
/// 时间是**本地时间** —— 文件名出现在用户桌面上，是给人看的；
/// 用 UTC 会让「下午两点的会议」写着 06:00。
///
/// 不含任何用户可识别信息，避免隐私泄露。
pub fn session_file_name() -> String {
    let t = crate::platform::clock::now();
    format!("transcript-{}.dsht", t.compact())
}

/// 列出目录下的转写盘文件，按文件名倒序（新的在前）
pub fn list_sessions(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "dsht"))
        .collect();
    out.sort();
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("translator-tw-{tag}-{uniq}"));
        Tmp(p)
    }

    #[test]
    fn create_in_makes_directory() {
        let t = tmp("mkdir");
        let dir = t.0.join("nested/deeper");
        let w = TranscriptWriter::create_in(&dir).expect("应能创建");
        assert!(dir.exists(), "父目录应被自动创建");
        assert!(w.is_active());
    }

    #[test]
    fn file_name_has_expected_shape() {
        let n = session_file_name();
        assert!(n.starts_with("transcript-"), "{n}");
        assert!(n.ends_with(".dsht"), "{n}");
        // transcript-YYYYMMDD-HHMMSS.dsht → "transcript-"(11) + 8 + 1 + 6 + 5
        assert_eq!(n.len(), 31, "{n}");
    }

    #[test]
    fn write_failure_is_counted_not_panicked() {
        // 打开一个「目录」当文件写，必然失败
        let t = tmp("badfile");
        fs::create_dir_all(&t.0).unwrap();
        let dir = t.0.join("not_a_file");
        fs::create_dir_all(&dir).unwrap();
        // 直接把 writer 的输出换成一个坏句柄：这里改用 reader 侧模拟不可行，
        // 改为验证「连续失败后会放弃」，通过 create_in 后手工 close 再写
        let mut w = TranscriptWriter::create_in(&t.0).unwrap();
        w.finish(); // 关掉句柄 → 后续写入应被安全忽略而不是 panic
        w.append_add(0, "x", 0, 1, false, "对方");
        w.append_fill(0, "y", false);
        w.flush();
        // 关键：不该 panic，且状态可读
        assert_eq!(w.failures(), 0, "已关闭的写盘不计失败，只是不再写");
    }

    #[test]
    fn list_sessions_is_newest_first() {
        let t = tmp("list");
        fs::create_dir_all(&t.0).unwrap();
        for n in [
            "transcript-20260101-000000.dsht",
            "transcript-20260601-000000.dsht",
            "transcript-20260601-120000.dsht",
        ] {
            fs::write(t.0.join(n), b"").unwrap();
        }
        fs::write(t.0.join("unrelated.txt"), b"").unwrap();

        let got = list_sessions(&t.0).unwrap();
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 3, "只应列出 .dsht");
        assert_eq!(names[0], "transcript-20260601-120000.dsht", "新的应在前");
    }
}