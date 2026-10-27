//! 转写盘记录格式。
//!
//! ## 布局
//!
//! ```text
//! [header 40B 定长] [payload N 字节]
//!  seq u64 | start_ms u64 | end_ms u64 | payload_len u32
//!  flags u16 | kind u16    | reserved u32 | crc32 u32
//! ```
//!
//! ## 为什么是「定长头 + 变长载荷」而不是纯 JSONL
//!
//! 回溯历史时要跳过窗口外的成千上万条记录。纯 JSONL 必须**逐条解析 JSON**
//! 才能知道下一条在哪；定长头只需累加 `40 + payload_len` 就能跳，
//! 解析前就能定位目标范围 —— 在 1 小时会话（~1200 条）上差别不大，
//! 在 8 小时会议（~10000 条）上是数量级的差别。
//!
//! ## 为什么译文用「追加 patch」而不是原地改
//!
//! append-only 在断电时只会「末尾少一条」，而原地改可能写到一半，
//! 留下半条损坏记录却看不出异常。读取时合并 patch 即可，代价可忽略。

/// 定长头字节数
pub const HEADER_LEN: usize = 40;

/// 记录类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// 一行 EN 就位
    Add = 0,
    /// 译文回填
    Fill = 1,
    /// 标记待复核
    Rev = 2,
}

impl Kind {
    fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Kind::Add),
            1 => Some(Kind::Fill),
            2 => Some(Kind::Rev),
            _ => None,
        }
    }
}

/// 行标志位
pub mod flags {
    /// 该段是被强制切分的
    pub const FORCED_CUT: u16 = 1 << 0;
    /// 需人工复核
    pub const NEEDS_REVIEW: u16 = 1 << 1;
}

/// 一条完整记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub flags: u16,
    pub kind: Kind,
    pub payload: String,
}

impl Record {
    /// 一行 EN 就位。
    ///
    /// `payload` 布局：`<speaker>\t<source>`。
    ///
    /// 说话人必须落盘 —— 否则从文件回读的历史丢失了「谁说的」，
    /// 多声源场景下导出的记录无法区分对话双方。
    /// 旧文件（payload 无 tab）读出说话人为空，向后兼容。
    pub fn add(
        seq: u64,
        start_ms: u64,
        end_ms: u64,
        source: &str,
        forced_cut: bool,
        speaker: &str,
    ) -> Self {
        let mut f = 0u16;
        if forced_cut {
            f |= flags::FORCED_CUT;
        }
        Self {
            seq,
            start_ms,
            end_ms,
            flags: f,
            kind: Kind::Add,
            payload: format!("{}\t{}", escape(speaker), escape(source)),
        }
    }

    pub fn fill(seq: u64, target: &str, needs_review: bool) -> Self {
        let mut f = 0u16;
        if needs_review {
            f |= flags::NEEDS_REVIEW;
        }
        Self {
            seq,
            start_ms: 0,
            end_ms: 0,
            flags: f,
            kind: Kind::Fill,
            // 载荷第二个字段是 review 标志，合并时用它更新 flags
            payload: format!("{}\t{}", escape(target), if needs_review { 1 } else { 0 }),
        }
    }

    pub fn forced_cut(&self) -> bool {
        self.flags & flags::FORCED_CUT != 0
    }

    pub fn needs_review(&self) -> bool {
        self.flags & flags::NEEDS_REVIEW != 0
    }

    /// `Kind::Fill` 载荷里的译文（已反转义）
    pub fn fill_target(&self) -> String {
        let first = self.payload.split('\t').next().unwrap_or("");
        unescape(first)
    }

    /// `Kind::Add` 载荷里的说话人与原文。
    ///
    /// 载荷是 `<speaker>\t<source>`；旧文件无 tab → 说话人空、原文整体。
    pub fn add_parts(&self) -> (String, String) {
        match self.payload.split_once('\t') {
            Some((spk, src)) => (unescape(spk), unescape(src)),
            None => (String::new(), unescape(&self.payload)),
        }
    }

    /// 序列化为「头 + 载荷」
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.payload.as_bytes();
        let crc = crc32(payload);
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());

        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.start_ms.to_le_bytes());
        out.extend_from_slice(&self.end_ms.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&(self.kind as u16).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved，留给将来的扩展
        out.extend_from_slice(&crc.to_le_bytes());
        debug_assert_eq!(out.len(), HEADER_LEN);
        out.extend_from_slice(payload);
        out
    }
}

/// 解码结果
#[derive(Debug)]
pub enum Decoded {
    Ok(Record),
    /// 尾部被截断（断电 / 进程被杀）。调用方应停止扫描并保留已解出的记录。
    Truncated,
}

/// 从 `buf` 的 `pos` 处解出一条记录，返回记录与下一位置
pub fn decode(buf: &[u8], pos: usize) -> Option<(Decoded, usize)> {
    if pos + HEADER_LEN > buf.len() {
        return None;
    }
    let h = &buf[pos..pos + HEADER_LEN];
    let g8 = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap());
    let g4 = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let g2 = |i: usize| u16::from_le_bytes(h[i..i + 2].try_into().unwrap());

    let seq = g8(0);
    let start_ms = g8(8);
    let end_ms = g8(16);
    let payload_len = g4(24) as usize;
    let flag_bits = g2(28);
    let kind_raw = g2(30);
    let crc = g4(36);

    let body = pos + HEADER_LEN;
    if body + payload_len > buf.len() {
        // 载荷不完整 —— 断电截断，返回 Truncated 让上层优雅收手
        return Some((Decoded::Truncated, buf.len()));
    }
    let payload_bytes = &buf[body..body + payload_len];

    if crc32(payload_bytes) != crc {
        return Some((Decoded::Truncated, buf.len()));
    }
    let Some(kind) = Kind::from_u16(kind_raw) else {
        // 未知类型：无法解析，跳过这条载荷继续往后找
        return Some((Decoded::Truncated, body + payload_len));
    };

    let payload = String::from_utf8_lossy(payload_bytes).into_owned();
    Some((
        Decoded::Ok(Record {
            seq,
            start_ms,
            end_ms,
            flags: flag_bits,
            kind,
            payload,
        }),
        body + payload_len,
    ))
}

/// 转义载荷里的分隔符与换行
///
/// 记录本身已经是二进制安全的，但 payload 要能直接被记事本查看与排查，
/// 所以把 tab / 换行 / 反斜杠转成 `\t` `\n` `\\` 两字符序列。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// 反转义
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            // 结尾孤立的反斜杠：原样保留，不丢用户文本
            None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// CRC-32（IEEE 802.3），手写表法，零依赖
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });

    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// JSON 字符串转义。
///
/// FFI 层的 `translator_transcript_query` 要把行以 JSON 返回给 Dart，
/// 故把转义函数放在这里一并维护 —— 免得转写盘与 FFI 各写一套规则。
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // 控制字符必须转义，否则 JSON 非法、Dart jsonDecode 会抛
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_exactly_40_bytes() {
        let r = Record::add(7, 100, 200, "hello", false, "对方");
        let enc = r.encode();
        assert_eq!(&enc[..HEADER_LEN].len(), &HEADER_LEN);
        assert!(enc.len() > HEADER_LEN);
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let r = Record::add(42, 1000, 2000, "we deploy Kubernetes", true, "对方");
        let enc = r.encode();
        let (dec, next) = decode(&enc, 0).unwrap();
        assert_eq!(next, enc.len());
        match dec {
            Decoded::Ok(got) => {
                assert_eq!(got.seq, 42);
                assert_eq!(got.start_ms, 1000);
                assert_eq!(got.end_ms, 2000);
                assert!(got.forced_cut());
                let (spk, src) = got.add_parts();
                assert_eq!(spk, "对方");
                assert_eq!(src, "we deploy Kubernetes");
            }
            Decoded::Truncated => panic!("不该被判定为截断"),
        }
    }

    #[test]
    fn tab_and_newline_survive_roundtrip() {
        let nasty = "第一行\n第二行\t带制表\\和反斜杠";
        let r = Record::add(1, 0, 1, nasty, false, "对方");
        let enc = r.encode();
        let (dec, _) = decode(&enc, 0).unwrap();
        match dec {
            Decoded::Ok(got) => {
                let (spk, src) = got.add_parts();
                assert_eq!(spk, "对方");
                assert_eq!(src, nasty);
            }
            Decoded::Truncated => panic!("不该被截断"),
        }
    }

    #[test]
    fn utf8_multibyte_survives() {
        // 中文按字节算长度是 3 倍，定长头的 payload_len 存的是字节数
        let r = Record::add(1, 0, 1, "我们今天部署 Kubernetes 到生产环境", false, "对方");
        let enc = r.encode();
        let (dec, next) = decode(&enc, 0).unwrap();
        assert_eq!(next, enc.len());
        match dec {
            Decoded::Ok(got) => {
                let (_spk, src) = got.add_parts();
                assert_eq!(src, "我们今天部署 Kubernetes 到生产环境");
            }
            Decoded::Truncated => panic!("不该被截断"),
        }
    }

    #[test]
    fn truncated_tail_is_detected_not_panicked() {
        // 模拟断电：只写了一半载荷
        let mut enc = Record::add(1, 0, 10, "hello world", false, "对方").encode();
        enc.truncate(HEADER_LEN + 3);
        match decode(&enc, 0) {
            Some((Decoded::Truncated, _)) => {}
            other => panic!("应判定为截断，得到 {other:?}"),
        }
    }

    #[test]
    fn header_shorter_than_40_is_none() {
        let buf = vec![0u8; 12];
        assert!(decode(&buf, 0).is_none());
    }

    #[test]
    fn corrupted_payload_fails_crc() {
        let mut enc = Record::add(1, 0, 10, "hello", false, "对方").encode();
        // 翻转载荷里的一个字节
        let last = enc.len() - 1;
        enc[last] ^= 0xFF;
        match decode(&enc, 0) {
            Some((Decoded::Truncated, _)) => {}
            other => panic!("CRC 应拦住损坏，得到 {other:?}"),
        }
    }

    #[test]
    fn fill_record_carries_review_flag() {
        let r = Record::fill(9, "我们部署 Kubernetes", true);
        assert_eq!(r.kind, Kind::Fill);
        assert!(r.needs_review());
        assert_eq!(r.fill_target(), "我们部署 Kubernetes");
    }

    #[test]
    fn legacy_payload_without_tab_is_read_as_all_source() {
        // 旧版写入的 Add 记录 payload 就是纯原文（没有说话人）。
        // 读回时不能把原文劈成两半 —— 说话人留空即可。
        let mut r = Record::add(0, 0, 100, "hello", false, "对方");
        r.payload = "hello".to_string(); // 模拟旧文件
        let (spk, src) = r.add_parts();
        assert_eq!(spk, "");
        assert_eq!(src, "hello");
    }

    #[test]
    fn speaker_containing_tab_does_not_break_split() {
        // 分隔符必须是**第一个真 tab**。说话人里的真 tab 已被 escape
        // 成两字符的 \\t，不应被当成分隔符。
        let r = Record::add(0, 0, 100, "source", false, "a\tb");
        let (spk, src) = r.add_parts();
        assert_eq!(spk, "a\tb");
        assert_eq!(src, "source");
    }

    #[test]
    fn escape_is_reversible_for_all_specials() {
        for s in ["a\tb", "a\nb", "a\\b", "a\r\nb", "普通文本", ""] {
            assert_eq!(unescape(&escape(s)), s, "反转义失败: {s:?}");
        }
    }

    #[test]
    fn json_escape_handles_control_chars() {
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_escape("a\nb"), "\"a\\nb\"");
        // 0x01 是控制字符，必须转义否则 JSON 非法
        assert_eq!(json_escape("\u{1}"), "\"\\u0001\"");
        // 中文不该被转义（UTF-8 直接放行）
        assert_eq!(json_escape("中文"), "\"中文\"");
    }

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32/ISO-HDLC 对 "123456789" 的标准值
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32(b""), 0);
    }
}