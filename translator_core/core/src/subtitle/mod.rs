//! 字幕聚合：partial → final 的稳定段协调。
//!
//! ## 核心设计决定（DESIGN.md §6.2）
//!
//! **partial 结果不直接进入正式字幕。**
//!
//! 流式识别器在句子中途会大幅改写已输出的前文（"I think" → "I think we should"
//! → "I think we should probably"）。若直接显示，字幕会剧烈跳动，
//! 这是流式字幕体验差的头号原因。
//!
//! 因此：partial 只用于"预览态"（UI 以灰色斜体渲染），
//! **只有 VAD 判定断句后的 final 段才成为正式字幕行**。

/// 说话人标签，缺省「对方」（单声源时可能是 None）
fn speaker_of(line: &SubtitleLine) -> &str {
    line.speaker.as_deref().unwrap_or("对方")
}

/// ms → `HH:MM:SS`（导出可读格式，比 SRT 的 `HH:MM:SS,mmm` 更适合阅读）
fn hhmmss(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    format!("{h:02}:{m:02}:{s:02}")
}

/// 一条字幕行
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleLine {
    /// 全局单调行号（永不复用）。
    ///
    /// 它是这一行在**整个会话**里的稳定身份，与内存窗口无关 ——
    /// 窗口丢弃旧行时索引会变，seq 不会。转写盘与内存窗口之间
    /// 的映射全靠它，所以 UI 回溯历史时能精确对应。
    pub seq: u64,
    /// 英文原文
    pub source: String,
    /// 中文译文（None = 翻译中或翻译失败）
    pub target: Option<String>,
    /// 起始时间戳（相对会话开始的毫秒）
    pub start_ms: u64,
    /// 结束时间戳
    pub end_ms: u64,
    /// 因哨兵往返校验失败而需人工复核（GLOSSARY.md §4.3）
    pub needs_review: bool,
    /// 该段是被强制切分的（超长段）
    pub forced_cut: bool,
    /// 发言人标签（diarization 启用时填充）
    pub speaker: Option<String>,
}

impl SubtitleLine {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    /// 翻译是否已就绪
    pub fn is_translated(&self) -> bool {
        self.target.is_some()
    }

    /// 是否仍在等待翻译
    pub fn is_translating(&self) -> bool {
        self.target.is_none() && !self.needs_review
    }
}

/// 预览态（未确认的 partial）
#[derive(Debug, Clone, PartialEq)]
pub struct PartialText {
    pub text: String,
    pub start_ms: u64,
}

/// UI 渲染视图
#[derive(Debug, Clone, Default)]
pub struct SubtitleView {
    pub lines: Vec<SubtitleLine>,
    pub partial: Option<PartialText>,
    /// 时间轴总长
    pub duration_ms: u64,
}

/// 字幕聚合器。
///
/// 状态：已确认的行 + 正在进行的一段。
/// 会话时间轴统一用「相对会话开始的毫秒」，保证三端一致（§7.2）。
pub struct SubtitleAssembler {
    lines: Vec<SubtitleLine>,
    partial: Option<PartialText>,
    /// 会话起点（单调时钟）
    session_start_ms: u64,
    /// 最后一段语音的起始偏移，用于未完成段的占位
    last_speech_start_ms: Option<u64>,
    /// 最大保留行数，防止长会话内存无限增长
    max_lines: usize,
    /// 下一条要分配的行号。全局单调、永不复用。
    next_seq: u64,
    /// 当前内存窗口首行的全局 seq。
    ///
    /// 有了它，`seq → 窗口内下标` 就是一次减法，
    /// 不会因为滑动窗口丢行而错位。
    base_seq: u64,
}

impl SubtitleAssembler {
    pub fn new(session_start_ms: u64) -> Self {
        Self {
            lines: Vec::new(),
            partial: None,
            session_start_ms,
            last_speech_start_ms: None,
            max_lines: 10_000,
            next_seq: 0,
            base_seq: 0,
        }
    }

    /// 下一条将被分配的 seq
    pub fn peek_next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 当前内存窗口首行的 seq。seq 小于它的行已被丢弃（转写盘仍保留）。
    pub fn base_seq(&self) -> u64 {
        self.base_seq
    }

    /// 调整内存窗口上限。
    ///
    /// 必须早于任何 commit 调用：缩窗会立即丢弃队首，
    /// 若此时已有译文在途，其 seq 会落到窗口之外（见 [`Self::fill_translation`]）。
    pub fn set_max_lines(&mut self, max_lines: usize) {
        self.max_lines = max_lines.max(1);
        self.trim();
    }

    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// 丢弃超出窗口的旧行，并同步推进 `base_seq`。
    fn trim(&mut self) {
        let excess = self.lines.len().saturating_sub(self.max_lines);
        if excess > 0 {
            self.lines.drain(0..excess);
            self.base_seq += excess as u64;
        }
    }

    /// 全局 seq → 窗口内下标。超出窗口返回 None（该行已被丢弃）。
    #[inline]
    fn index_of(&self, seq: u64) -> Option<usize> {
        seq.checked_sub(self.base_seq)
            .map(|i| i as usize)
            .filter(|&i| i < self.lines.len())
    }

    /// 单调时钟毫秒 → 会话相对偏移
    pub fn rel(&self, abs_ms: u64) -> u64 {
        abs_ms.saturating_sub(self.session_start_ms)
    }

    /// 送入 partial 结果（仅更新预览态，不产生正式行）
    pub fn push_partial(&mut self, text: String, abs_time_ms: u64) {
        let start = self.last_speech_start_ms.unwrap_or(abs_time_ms);
        self.partial = Some(PartialText {
            text,
            start_ms: self.rel(start),
        });
    }

    /// 提交一个 final 段成为正式字幕行。
    ///
    /// `mt_pending` 为 true 表示译文尚未就绪，先入行再后补（MT 背压，§6.2）。
    pub fn commit(
        &mut self,
        source: String,
        target: Option<String>,
        abs_start_ms: u64,
        abs_end_ms: u64,
        forced_cut: bool,
        needs_review: bool,
        speaker: Option<String>,
    ) -> u64 {
        let start = self.last_speech_start_ms.unwrap_or(abs_start_ms);
        let seq = self.next_seq;
        self.next_seq += 1;
        let line = SubtitleLine {
            seq,
            source,
            target,
            start_ms: self.rel(start),
            end_ms: self.rel(abs_end_ms),
            needs_review,
            forced_cut,
            speaker,
        };
        self.lines.push(line);
        self.partial = None;
        self.last_speech_start_ms = None;

        self.trim();
        seq
    }

    /// 回填译文（MT 异步完成后调用）
    ///
    /// `seq` 是 [`Self::commit`] 返回的全局行号，不是数组下标 —— 这一点很关键：
    /// 旧实现返回 `lines.len()`，而滑动窗口 `drain` 会把所有下标前移，
    /// 导致长会话里译文被写到**另一句话**上。行号永不复用，不受丢行影响。
    ///
    /// 行若已因超出内存窗口被丢弃，返回 false（转写盘仍保留该行）。
    pub fn fill_translation(&mut self, seq: u64, target: String, needs_review: bool) -> bool {
        match self.index_of(seq) {
            Some(i) => {
                let line = &mut self.lines[i];
                line.target = Some(target);
                if needs_review {
                    line.needs_review = true;
                }
                true
            }
            None => false,
        }
    }

    /// 标记某行需人工复核
    pub fn flag_for_review(&mut self, seq: u64) -> bool {
        match self.index_of(seq) {
            Some(i) => {
                self.lines[i].needs_review = true;
                true
            }
            None => false,
        }
    }

    /// 会话结束，冲刷残留 partial
    pub fn finish(&mut self) {
        self.partial = None;
    }

    pub fn view(&self) -> SubtitleView {
        let duration = self
            .lines
            .last()
            .map(|l| l.end_ms)
            .or_else(|| self.partial.as_ref().map(|p| p.start_ms))
            .unwrap_or(0);
        SubtitleView {
            lines: self.lines.clone(),
            partial: self.partial.clone(),
            duration_ms: duration,
        }
    }

    pub fn lines(&self) -> &[SubtitleLine] {
        &self.lines
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 导出 SRT
    pub fn export_srt(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            out.push_str(&format!("{}\n", i + 1));
            out.push_str(&format!(
                "{} --> {}\n",
                srt_timestamp(line.start_ms),
                srt_timestamp(line.end_ms)
            ));
            out.push_str(&line.source);
            out.push('\n');
            if let Some(t) = &line.target {
                out.push_str(t);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    /// 导出 VTT
    pub fn export_vtt(&self) -> String {
        let mut out = String::from("WEBVTT\n\n");
        for line in &self.lines {
            out.push_str(&format!(
                "{} --> {}\n",
                vtt_timestamp(line.start_ms),
                vtt_timestamp(line.end_ms)
            ));
            out.push_str(&line.source);
            out.push('\n');
            if let Some(t) = &line.target {
                out.push_str(t);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    /// 导出纯文本对照（`.txt`）。
    ///
    /// 格式是**给人读的对话记录**，而不是给程序解析的：
    /// 每行带时间戳、说话人、原文、译文。多声源时说话人必不可少 ——
    /// 否则「我方说的」和「对方说的」混在一起，读起来分不清是谁。
    ///
    /// ```text
    /// [00:00:12] 对方
    ///   EN: we deploy Kubernetes today
    ///   ZH: 我们今天部署 Kubernetes
    ///
    /// [00:00:15] 我
    ///   EN: yeah let's do it
    ///   ZH: 好的，开始吧
    /// ```
    pub fn export_txt(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&format!("[{}] {}\n", hhmmss(line.start_ms), speaker_of(line)));
            out.push_str(&format!("  EN: {}\n", line.source));
            match &line.target {
                Some(t) => out.push_str(&format!("  ZH: {t}\n")),
                None => out.push_str("  ZH: （翻译中）\n"),
            }
            if line.needs_review {
                out.push_str("  ⚠ 术语待复核\n");
            }
            out.push('\n');
        }
        out
    }

    /// 统计：待翻译行数 / 需复核行数
    pub fn stats(&self) -> SubtitleStats {
        SubtitleStats {
            total: self.lines.len(),
            translating: self.lines.iter().filter(|l| l.is_translating()).count(),
            needs_review: self.lines.iter().filter(|l| l.needs_review).count(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtitleStats {
    pub total: usize,
    pub translating: usize,
    pub needs_review: usize,
}

/// ms → `HH:MM:SS,mmm`（SRT）
pub fn srt_timestamp(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let milli = ms % 1000;
    format!("{h:02}:{m:02}:{s:02},{milli:03}")
}

/// ms → `HH:MM:SS.mmm`（VTT）
pub fn vtt_timestamp(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let milli = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{milli:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_does_not_create_lines() {
        let mut a = SubtitleAssembler::new(0);
        a.push_partial("I think".into(), 0);
        a.push_partial("I think we should".into(), 200);
        assert_eq!(a.line_count(), 0, "partial 不得产生正式行");
        assert!(a.view().partial.is_some());
    }

    #[test]
    fn commit_creates_line_and_clears_partial() {
        let mut a = SubtitleAssembler::new(1000);
        a.push_partial("hello".into(), 1000);
        a.commit("hello world".into(), None, 1000, 1800, false, false, None);
        assert_eq!(a.line_count(), 1);
        assert!(a.view().partial.is_none());
        assert_eq!(a.lines()[0].source, "hello world");
    }

    #[test]
    fn timestamps_are_relative_to_session_start() {
        // 会话起点 5000，第一条从 7000 开始 → 相对 2000
        let mut a = SubtitleAssembler::new(5000);
        a.commit("x".into(), None, 7000, 8000, false, false, None);
        assert_eq!(a.lines()[0].start_ms, 2000);
        assert_eq!(a.lines()[0].end_ms, 3000);
    }

    #[test]
    fn translation_backfill_updates_line() {
        let mut a = SubtitleAssembler::new(0);
        let idx = a.commit("deploying Kubernetes".into(), None, 0, 1000, false, false, None);
        assert!(a.lines()[0].is_translating());

        a.fill_translation(idx, "正在部署 Kubernetes".into(), false);
        assert_eq!(
            a.lines()[0].target.as_deref(),
            Some("正在部署 Kubernetes")
        );
        assert!(a.lines()[0].is_translated());
    }

    #[test]
    fn review_flag_survives_translation_backfill() {
        let mut a = SubtitleAssembler::new(0);
        let idx = a.commit("K8s".into(), None, 0, 100, false, false, None);
        a.fill_translation(idx, "K8s".into(), true);
        assert!(a.lines()[0].needs_review);
    }

    #[test]
    fn mt_backpressure_does_not_block_line_display() {
        // 核心测试：翻译慢时，英文必须已经上屏
        let mut a = SubtitleAssembler::new(0);
        let i1 = a.commit("first".into(), None, 0, 500, false, false, None);
        a.commit("second".into(), None, 500, 1000, false, false, None);
        // 此时两行都已显示，只是都没译文
        assert_eq!(a.line_count(), 2);
        assert_eq!(a.stats().translating, 2);
        // 译文乱序到达
        a.fill_translation(i1, "第一".into(), false);
        assert_eq!(a.stats().translating, 1);
    }

    #[test]
    fn consecutive_lines_use_previous_speech_start() {
        // ASR 的 start 时间戳可能不准，用上一段语音起点兜底
        let mut a = SubtitleAssembler::new(0);
        a.last_speech_start_ms = Some(100);
        a.commit("text".into(), None, 500, 900, false, false, None);
        assert_eq!(a.lines()[0].start_ms, 100);
    }

    #[test]
    fn max_lines_drops_oldest() {
        let mut a = SubtitleAssembler::new(0);
        a.max_lines = 3;
        for i in 0..5 {
            a.commit(format!("line{i}"), None, i * 100, i * 100 + 50, false, false, None);
        }
        assert_eq!(a.line_count(), 3);
        assert_eq!(a.lines()[0].source, "line2");
    }

    #[test]
    fn srt_format_is_valid() {
        let mut a = SubtitleAssembler::new(0);
        a.commit("Hello".into(), Some("你好".into()), 0, 2500, false, false, None);
        let srt = a.export_srt();
        assert!(srt.starts_with("1\n"));
        assert!(srt.contains("00:00:00,000 --> 00:00:02,500"));
        assert!(srt.contains("Hello"));
        assert!(srt.contains("你好"));
    }

    #[test]
    fn vtt_format_is_valid() {
        let mut a = SubtitleAssembler::new(0);
        a.commit("Hello".into(), None, 0, 1500, false, false, None);
        let vtt = a.export_vtt();
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:00.000 --> 00:00:01.500"));
    }

    #[test]
    fn txt_export_is_readable_dialogue_record() {
        // TXT 导出是**给人读的对话记录**：必须含时间戳、说话人、原文、译文。
        // 旧格式只有 `源文\t译文`，多声源时根本分不清谁说的。
        let mut a = SubtitleAssembler::new(0);
        a.commit(
            "Hello".into(),
            Some("你好".into()),
            1_500,
            2_000,
            false,
            false,
            Some("对方".into()),
        );
        let txt = a.export_txt();
        assert!(txt.contains("[00:00:01]"), "缺时间戳: {txt}");
        assert!(txt.contains("对方"), "缺说话人: {txt}");
        assert!(txt.contains("EN: Hello"), "缺原文: {txt}");
        assert!(txt.contains("ZH: 你好"), "缺译文: {txt}");
    }

    #[test]
    fn txt_export_marks_untranslated_and_review() {
        let mut a = SubtitleAssembler::new(0);
        a.commit("Pending".into(), None, 0, 100, false, false, Some("我".into()));
        let i = a.commit("Suspect".into(), Some("可疑".into()), 200, 300, false, true, None);
        a.flag_for_review(i);
        let txt = a.export_txt();
        assert!(txt.contains("（翻译中）"), "未译行应有占位: {txt}");
        assert!(txt.contains("术语待复核"), "待复核行应标注: {txt}");
        // 单声源且无 speaker 时回落到「对方」，不显示空白
        assert!(txt.contains("对方"), "{txt}");
    }

    #[test]
    fn timestamp_formats() {
        assert_eq!(srt_timestamp(0), "00:00:00,000");
        assert_eq!(srt_timestamp(3_661_500), "01:01:01,500");
        assert_eq!(vtt_timestamp(3_661_500), "01:01:01.500");
    }

    #[test]
    fn view_duration_reflects_last_line() {
        let mut a = SubtitleAssembler::new(0);
        a.commit("a".into(), None, 0, 5000, false, false, None);
        assert_eq!(a.view().duration_ms, 5000);
    }

    #[test]
    fn review_stats_correct() {
        let mut a = SubtitleAssembler::new(0);
        a.commit("a".into(), Some("甲".into()), 0, 100, false, false, None);
        a.commit("b".into(), None, 100, 200, false, false, None);
        let i = a.commit("c".into(), None, 200, 300, false, false, None);
        a.flag_for_review(i);
        let s = a.stats();
        assert_eq!(s.total, 3);
        assert_eq!(s.translating, 1);
        assert_eq!(s.needs_review, 1);
    }

    // ── seq 身份：滑动窗口下的回填正确性 ──────────────────────────
    //
    // 下面是本次改造要修的真 bug 的回归测试。
    // 旧实现里 commit 返回 `lines.len()`（入队前的下标），而滑动窗口
    // drain 会把所有下标前移，于是「译文回填」会写到**另一句话**上：
    //     提交 line0..line4（窗口 3）→ 丢弃 line0、line1
    //     旧下标 3 此刻指向 line3，但它本该是 line3（碰巧对）
    //     旧下标 1 此刻指向 line2，它本该是 line1 ← 错位
    // 该错位只在「超过窗口上限」且「译文未回」同时成立时暴露，
    // 所以旧测试 max_lines_drops_oldest 覆盖不到。

    #[test]
    fn seq_survives_sliding_window() {
        let mut a = SubtitleAssembler::new(0);
        a.set_max_lines(3);

        let seqs: Vec<u64> = (0..5)
            .map(|i| a.commit(format!("line{i}"), None, i * 100, i * 100 + 50, false, false, None))
            .collect();

        // seq 是全局单调行号，与窗口无关
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
        assert_eq!(a.base_seq(), 2, "前两行已被丢弃，base_seq 应前移");

        // 回填第一行：它已在窗口外，应返回 false 而非写错行
        assert!(!a.fill_translation(seqs[0], "第零行".into(), false));

        // 回填仍在窗口内的行 —— 必须落在正确的句子上
        assert!(a.fill_translation(seqs[3], "第三行".into(), false));
        let line = a.lines().iter().find(|l| l.seq == 3).unwrap();
        assert_eq!(line.source, "line3");
        assert_eq!(line.target.as_deref(), Some("第三行"));
    }

    #[test]
    fn fill_translation_never_crosses_lines() {
        // 专门盯住「写到别的行」这个症状：回填后逐行核对
        let mut a = SubtitleAssembler::new(0);
        a.set_max_lines(2);
        for i in 0..10 {
            a.commit(format!("line{i}"), None, i * 100, i * 100 + 50, false, false, None);
        }
        // 最后 2 行
        assert_eq!(a.line_count(), 2);
        let last = a.peek_next_seq() - 1;
        assert!(a.fill_translation(last, "最后一行的译文".into(), false));
        for l in a.lines() {
            match l.target.as_deref() {
                Some(t) => assert_eq!(t, "最后一行的译文", "译文落到了错误的行: seq={}", l.seq),
                None => {}
            }
        }
        assert_eq!(a.lines()[1].target.as_deref(), Some("最后一行的译文"));
    }

    #[test]
    fn set_max_lines_trims_immediately() {
        let mut a = SubtitleAssembler::new(0);
        for i in 0..10 {
            a.commit(format!("line{i}"), None, i * 100, i * 100 + 50, false, false, None);
        }
        assert_eq!(a.line_count(), 10);
        a.set_max_lines(3);
        assert_eq!(a.line_count(), 3);
        assert_eq!(a.base_seq(), 7);
        assert_eq!(a.lines()[0].seq, 7, "窗口首行 seq 应与 base_seq 一致");
    }

    #[test]
    fn seq_is_never_reused_after_trim() {
        // 丢行后继续提交，新 seq 必须接着走，不能被回收
        let mut a = SubtitleAssembler::new(0);
        a.set_max_lines(2);
        for i in 0..6 {
            a.commit(format!("line{i}"), None, i * 100, i * 100 + 50, false, false, None);
        }
        let n = a.peek_next_seq();
        assert_eq!(n, 6);
        let s = a.commit("after".into(), None, 600, 700, false, false, None);
        assert_eq!(s, 6, "新行 seq 必须大于所有历史 seq");
    }
}
