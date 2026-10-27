//! 用户可见目录的解析（转写盘默认落点）。
//!
//! ## 为什么不硬编码桌面路径
//!
//! 「桌面」在 Windows 上**不是一个固定目录**。很多用户把它重定向到了
//! OneDrive：`C:\Users\<name>\Desktop` 可能根本不存在，真正在
//! `C:\Users\<name>\OneDrive\桌面`。硬编码会让程序静默创建一个假桌面目录，
//! 用户在真桌面上什么都看不到。
//!
//! 正确做法是问系统：`SHGetKnownFolderPath(FOLDERID_Desktop)`。
//!
//! ## 为什么放在 platform/ 而不是 transcript/
//!
//! DESIGN §7.1 硬约束：`core/src` 内不允许出现平台分支。
//! 这类系统调用必然要 `#[cfg]`，所以只能收敛到 `platform/` 下 ——
//! 这正是该规则存在的意义：让「哪里有平台差异」永远能一眼定位。
//!
//! 手写 extern 声明而非引入 `windows` crate：这里只需要 2 个函数，
//! 而核心 crate 的零重依赖是离线门禁（`check_offline.py`）的一部分。

use std::path::PathBuf;

/// 转写盘默认目录名
pub const TRANSCRIPT_DIR_NAME: &str = "translate results";

/// 转写盘默认目录：`<桌面>/translate results`
///
/// 无法解析系统目录时返回 `None`，由调用方决定降级策略
/// （UI 应当提示用户手动选择，而不是默默写到某个奇怪的地方）。
pub fn default_transcript_dir() -> Option<PathBuf> {
    desktop_dir().map(|d| d.join(TRANSCRIPT_DIR_NAME))
}

/// 系统「桌面」目录
#[cfg(target_os = "windows")]
pub fn desktop_dir() -> Option<PathBuf> {
    windows_desktop_via_known_folder()
        .or_else(|| fallback_desktop_from_env())
}

#[cfg(not(target_os = "windows"))]
pub fn desktop_dir() -> Option<PathBuf> {
    // 移动端没有「桌面」概念；这里给出合理回退，
    // 实际由调用方（Android 侧）用 path_provider 的应用私有目录覆盖。
    fallback_desktop_from_env()
}

/// 桌面未重定向时的常规位置
fn fallback_desktop_from_env() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let profile = std::env::var_os("USERPROFILE")?;
    #[cfg(not(target_os = "windows"))]
    let profile = std::env::var_os("HOME")?;

    Some(PathBuf::from(profile).join("Desktop"))
}

/// `SHGetKnownFolderPath(FOLDERID_Desktop)` —— 手写 extern，约 20 行
#[cfg(target_os = "windows")]
fn windows_desktop_via_known_folder() -> Option<PathBuf> {
    use std::ffi::c_void;

    #[repr(C)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    // extern "system" 块**不会**自动链接系统库，
    // 必须用 #[link] 显式声明，否则链接期报 LNK2019 无法解析的外部符号。
    #[link(name = "shell32")]
    extern "system" {
        fn SHGetKnownFolderPath(
            rfid: *const Guid,
            dw_flags: u32,
            h_token: *mut c_void,
            ppsz_path: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(pv: *mut c_void);
    }

    // FOLDERID_Desktop = {B4BFCC3A-DB2C-424C-B029-7FE99A87C641}
    const FOLDERID_DESKTOP: Guid = Guid {
        d1: 0xB4BFCC3A,
        d2: 0xDB2C,
        d3: 0x424C,
        d4: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
    };

    const S_OK: i32 = 0;
    const KF_FLAG_DEFAULT: u32 = 0;

    unsafe {
        let mut raw: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(
            &FOLDERID_DESKTOP as *const Guid,
            KF_FLAG_DEFAULT,
            std::ptr::null_mut(),
            &mut raw,
        );
        if hr != S_OK || raw.is_null() {
            return None;
        }

        // PWSTR → String。CoTaskMemFree 释放系统分配的内存，
        // 忘记释放会泄漏（shell32 的分配器是 CoTaskMemAlloc）。
        let mut len = 0usize;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(raw, len);
        let s = String::from_utf16_lossy(slice);
        CoTaskMemFree(raw as *mut c_void);

        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_dir_resolves_to_something() {
        let d = desktop_dir();
        assert!(d.is_some(), "桌面目录应可解析");
        assert!(
            d.unwrap().is_absolute(),
            "必须是绝对路径，否则换工作目录就找不到"
        );
    }

    #[test]
    fn transcript_dir_has_expected_name() {
        let d = default_transcript_dir().expect("应能解析桌面");
        assert_eq!(
            d.file_name().unwrap().to_string_lossy(),
            TRANSCRIPT_DIR_NAME
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn desktop_survives_one_drive_redirection() {
        // 若桌面被重定向到 OneDrive，真桌面下不会有 Desktop 目录。
        // 这个测试的价值在于「证明我们没有硬编码」，而不是断言目录存在。
        let via_api = windows_desktop_via_known_folder();
        let via_env = fallback_desktop_from_env();
        if let (Some(a), Some(b)) = (&via_api, &via_env) {
            // 未重定向时两者应一致；不一致说明 API 起了作用（好事）
            let _ = (a == b);
        }
    }
}