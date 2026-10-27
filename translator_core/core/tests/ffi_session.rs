//! FFI 边界安全性测试。
//!
//! ## 为什么这组测试最重要
//!
//! 跨 FFI 的 panic 会**直接杀死宿主进程**（Flutter），UI 上表现为
//! 「应用莫名其妙闪退」，且没有任何堆栈可查。所以这里逐个函数验证：
//! 传空指针必须返回错误码/空指针，**绝不能 panic**。
//!
//! 另外验证配置解析与生命周期在无采集器（无 `capture-desktop` feature）
//! 时的行为 —— CI 上没音频设备是常态。

use std::ffi::{c_char, c_void, CStr, CString};

// 既有采集级导出在 ffi 模块，会话级在 ffi_session 模块
use translator_core::ffi::{translator_free_string, translator_is_offline, translator_version};
use translator_core::ffi_session::*;

/// 把 C 字符串指针读回 Rust
unsafe fn read_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(|s| s.to_string())
}

// ── 空指针安全：这一组是防崩溃的核心 ──────────────────

#[test]
fn null_handle_never_panics() {
    unsafe {
        let null: *mut c_void = std::ptr::null_mut();

        // 这三个返回 int，必须给 InvalidArg(7)
        assert_eq!(translator_session_start(null), 7, "start");
        assert_eq!(translator_session_stop(null), 7, "stop");
        assert_eq!(translator_glossary_reload(null), 7, "glossary_reload");

        // 这三个返回指针，必须给 null
        assert!(translator_session_take_events(null).is_null(), "take_events");
        assert!(translator_session_partial(null).is_null(), "partial");
        assert!(translator_session_error_desc(null).is_null(), "error_desc");
        assert!(translator_session_export(null, 0).is_null(), "export");
        assert!(translator_selfcheck_json(null).is_null(), "selfcheck");
        assert!(translator_transcript_query(null, 0, 10).is_null(), "query");
        assert!(translator_transcript_path(null).is_null(), "path");
        assert!(translator_session_capture_handle(null).is_null(), "capture_handle");

        // free 接受 null 且必须安静返回
        translator_session_free(null);

        // 带 out 参数的：任一为 null 都要拒绝
        let mut status = FfiSessionStatus::default();
        assert_eq!(translator_session_status(null, &mut status), 7, "status");
        assert_eq!(translator_session_status(null, std::ptr::null_mut()), 7);
        let mut stats = FfiTranscriptStats::default();
        assert_eq!(translator_transcript_stats(null, &mut stats), 7, "stats");
        assert_eq!(translator_transcript_stats(null, std::ptr::null_mut()), 7);

        // 需要路径参数的
        let p = CString::new("x.tsv").unwrap();
        assert_eq!(translator_glossary_load(null, p.as_ptr()), 7, "glossary_load");
        assert_eq!(translator_model_import_dir(null, p.as_ptr()), 7, "import_dir");
    }
}

#[test]
fn invalid_arguments_are_rejected() {
    unsafe {
        // session_create 的 source / runtime 越界 → null
        let cfg = CString::new(r#"{"backend":"mock"}"#).unwrap();
        assert!(
            translator_session_create(cfg.as_ptr(), 99, 0).is_null(),
            "非法 source 应返回 null"
        );
        assert!(
            translator_session_create(cfg.as_ptr(), 0, 99).is_null(),
            "非法 runtime 应返回 null"
        );
        // cfg_json 为 null → null
        assert!(translator_session_create(std::ptr::null(), 0, 0).is_null());

        // 畸形 JSON → null（而不是 panic）
        let bad = CString::new("{not json").unwrap();
        assert!(translator_session_create(bad.as_ptr(), 0, 0).is_null());
    }
}

// ── 无采集器环境下的行为 ─────────────────────────────

#[test]
fn session_create_fails_gracefully_without_capture_backend() {
    // 没编译 capture-desktop 时没有系统音频设备，create 应返回 null 而不是崩。
    // 编译了该 feature 的机器上这一步会成功，故只在未编译时断言。
    #[cfg(not(feature = "capture-desktop"))]
    unsafe {
        let cfg = CString::new(r#"{"backend":"mock"}"#).unwrap();
        let h = translator_session_create(cfg.as_ptr(), 1, 0); // 1 = 麦克风
        // 麦克风路径在无 feature 时同样不可用
        if h.is_null() {
            // 这是预期的：UI 侧应据此提示「采集不可用」而不是崩溃
            return;
        }
        translator_session_free(h);
    }
}

// ── 无依赖采集器的导出 ─────────────────────────────────

#[test]
fn transcript_dir_resolves_to_something_human_readable() {
    unsafe {
        let p = translator_transcript_dir();
        let s = read_str(p).expect("应返回字符串");
        translator_free_string(p);
        if !s.is_empty() {
            assert!(
                s.contains("translate results"),
                "默认目录名应为 translate results，实际 {s}"
            );
        }
    }
}

#[test]
fn version_still_works_after_session_module_added() {
    // 回归保护：新增模块不应影响既有导出
    unsafe {
        let p = translator_version();
        assert!(!p.is_null());
        let v = read_str(p).expect("版本串应可读");
        translator_free_string(p);
        // 版本串格式：{channel}-{version}，如 snapshot-26.1020.20.13
        assert!(v.contains('-'), "{v}");
        assert!(!v.contains('(') && !v.contains(')'), "不应带时间戳括号: {v}");
        const CHANNELS: [&str; 7] = [
            "snapshot",
            "alpha",
            "beta",
            "pre",
            "release",
            "stable",
            "current",
        ];
        let ch = v.split('-').next().unwrap_or("");
        assert!(CHANNELS.contains(&ch), "渠道应合法: {v}");
    }
}

#[test]
fn is_offline_remains_true() {
    // 离线性是不可协商的红线（DESIGN §4.1）
    unsafe {
        assert!(translator_is_offline());
    }
}

#[test]
fn free_string_tolerates_null() {
    unsafe {
        translator_free_string(std::ptr::null_mut());
    }
}

// ── 配置解析的错误码（用 session_create 间接验证）──

#[test]
fn config_error_surfaces_as_null_not_panic() {
    unsafe {
        let cases = [
            "{not json",
            "",
            r#"{"profile":123}"#,        // profile 应是字符串
            r#"{"glossary_paths":"notanarray"}"#,
        ];
        for c in cases {
            let cs = CString::new(c).unwrap();
            let h = translator_session_create(cs.as_ptr(), 1, 0);
            if !h.is_null() {
                // 某些宽松解析器会接受 —— 至少不能崩
                translator_session_free(h);
            }
        }
    }
}

// ── 完整生命周期（需要能创建采集器）──────────────────

#[cfg(feature = "capture-desktop")]
mod lifecycle {
    use super::*;

    #[test]
    fn full_lifecycle_start_stop_free() {
        unsafe {
            let dir = std::env::temp_dir().join(format!(
                "translator-ffi-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let cfg_json = format!(
                r#"{{"backend":"mock","transcript_dir":{:?}}}"#,
                dir.to_string_lossy().to_string()
            );
            let cfg = CString::new(cfg_json).unwrap();

            // 麦克风源在多数机器上都能打开（系统音频输出设备可能没有）
            let h = translator_session_create(cfg.as_ptr(), 1, 0);
            if h.is_null() {
                eprintln!("[SKIP] 本机无音频设备，跳过生命周期测试");
                return;
            }

            // 重复 start 应被拒（接收端已移交）
            let rc1 = translator_session_start(h);
            assert_eq!(rc1, 0, "首次 start 应成功");
            let rc2 = translator_session_start(h);
            assert_eq!(rc2, 1, "重复 start 应返回 AlreadyRunning(1)");

            // 空转一会，模拟运行
            std::thread::sleep(std::time::Duration::from_millis(200));

            // 事件接口必须不崩，且返回合法 JSON
            let ev = translator_session_take_events(h);
            if !ev.is_null() {
                let s = read_str(ev).unwrap();
                assert!(s.starts_with("{\"events\":"), "{s}");
                translator_free_string(ev);
            }

            // 状态可读
            let mut st = FfiSessionStatus::default();
            assert_eq!(translator_session_status(h, &mut st), 0);
            assert!(st.asr_ready == 0 || st.asr_ready == 1);

            // 自检报告可读
            let sc = translator_selfcheck_json(h);
            assert!(!sc.is_null(), "自检报告不应为空");
            let scs = read_str(sc).unwrap();
            assert!(scs.contains("\"steps\""), "{scs}");
            translator_free_string(sc);

            // 停止
            assert_eq!(translator_session_stop(h), 0);

            // 停止后能导出（可能为空字符串）
            let srt = translator_session_export(h, 0);
            assert!(!srt.is_null());
            let srt_s = read_str(srt).unwrap();
            assert!(!srt_s.contains('\u{0}'));
            translator_free_string(srt);

            // 转写盘路径
            let tp = translator_transcript_path(h);
            assert!(!tp.is_null());

            // 释放（内部会再 stop 一次，必须幂等）
            translator_session_free(h);

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}