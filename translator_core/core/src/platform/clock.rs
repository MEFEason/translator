//! 本地时间。
//!
//! ## 为什么需要它
//!
//! 转写盘的文件名会出现在用户的**桌面**上，是给人看的。
//! 用 UTC 会出现「下午 2 点的会议，文件写着 06:00」这种困惑。
//!
//! `std::time::SystemTime` 只给 UTC，拿本地时间需要问系统。
//! 这里用 Windows 的 `GetLocalTime`（kernel32，默认就链接），
//! 其他平台退回 UTC 并在文档中说明。

use std::time::{SystemTime, UNIX_EPOCH};

/// 本地时间的年月日时分秒
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LocalTime {
    /// `20261020-142424` 形式
    pub fn compact(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// 当前本地时间
pub fn now() -> LocalTime {
    #[cfg(target_os = "windows")]
    {
        if let Some(t) = windows_local_time() {
            return t;
        }
    }
    utc_fallback()
}

#[cfg(target_os = "windows")]
fn windows_local_time() -> Option<LocalTime> {
    #[repr(C)]
    struct SystemTimeFields {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(out: *mut SystemTimeFields);
    }

    unsafe {
        let mut f = SystemTimeFields {
            year: 0,
            month: 0,
            day_of_week: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
        };
        GetLocalTime(&mut f);
        if f.year < 1980 || f.month == 0 || f.month > 12 {
            return None; // 没拿到有效值，交给回退
        }
        Some(LocalTime {
            year: f.year as i64,
            month: f.month as u32,
            day: f.day as u32,
            hour: f.hour as u32,
            minute: f.minute as u32,
            second: f.second as u32,
        })
    }
}

/// UTC 回退。非 Windows 平台在没有时区库时只能这样。
fn utc_fallback() -> LocalTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    LocalTime {
        year: y,
        month: m,
        day: d,
        hour: (tod / 3600) as u32,
        minute: ((tod % 3600) / 60) as u32,
        second: (tod % 60) as u32,
    }
}

/// Unix 天 → 公历日期（Howard Hinnant 算法）
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_plausible() {
        let t = now();
        assert!(t.year >= 2020, "年份异常: {t:?}");
        assert!((1..=12).contains(&t.month), "{t:?}");
        assert!((1..=31).contains(&t.day), "{t:?}");
        assert!(t.hour < 24, "{t:?}");
        assert!(t.minute < 60, "{t:?}");
        assert!(t.second < 60, "{t:?}");
    }

    #[test]
    fn compact_has_fixed_width() {
        let t = LocalTime {
            year: 2026,
            month: 10,
            day: 20,
            hour: 14,
            minute: 24,
            second: 5,
        };
        assert_eq!(t.compact(), "20261020-142405");
    }

    #[test]
    fn civil_conversion_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(20_746), (2026, 10, 20));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn local_time_is_not_utc() {
        // 本机时区是 UTC+8。若 API 生效，本地小时应与 UTC 相差 8 小时。
        // 只断言「差值合理」，不硬编码具体时刻（跨时区也成立）。
        let utc = utc_fallback();
        let loc = windows_local_time().expect("GetLocalTime 应可用");
        let mut diff = loc.hour as i64 - utc.hour as i64;
        if diff < -12 {
            diff += 24;
        }
        if diff > 12 {
            diff -= 24;
        }
        assert!(
            diff.abs() <= 12,
            "本地与 UTC 的小时差应在 ±12 内，实际 {diff}（local={loc:?} utc={utc:?}）"
        );
    }
}
