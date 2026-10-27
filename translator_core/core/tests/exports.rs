//! FFI 导出完整性门禁。
//!
//! ## 为什么需要这个测试
//!
//! 2026-10-23 应用启动即崩溃：
//! `Failed to lookup symbol 'translator_model_import_dir'`。
//! 根因是那个函数**漏了 `#[no_mangle]`** —— Rust 侧能编译、能通过单元测试，
//! 但符号没导出，Dart 侧 `lookupFunction` 直接失败，**整个应用起不来**。
//!
//! 单元测试抓不到这类问题（它测的是 Rust 内部调用，不经过动态库符号表）。
//! 本测试在**构建后**检查 DLL 的导出表，与 Dart 侧需要的清单逐一对账。
//!
//! ## 运行
//!
//! 由 `scripts/package.sh` 的出厂验证环节调用；
//! 也可手工跑：
//!
//! ```text
//! cargo test --test exports -- --nocapture
//! ```

use std::path::PathBuf;

/// Dart 侧 `bridge.dart` 实际 `lookupFunction` 的符号清单。
///
/// 改 Dart 绑定时**必须**同步改这里，否则漏了符号要到运行时才炸。
const REQUIRED_EXPORTS: &[&str] = &[
    // 会话级（桌面端主链路）
    "translator_session_create",
    "translator_session_free",
    "translator_session_start",
    "translator_session_stop",
    "translator_session_pause",
    "translator_session_take_events",
    "translator_session_status",
    "translator_session_error_desc",
    "translator_session_partial",
    "translator_session_export",
    "translator_session_capture_capability",
    "translator_session_capture_handle",
    // 词库 / 模型
    "translator_glossary_load",
    "translator_glossary_reload",
    "translator_model_import_dir",
    "translator_selfcheck_json",
    // 转写盘
    "translator_transcript_dir",
    "translator_transcript_path",
    "translator_transcript_query",
    "translator_transcript_stats",
    // 采集级（Android 兼容保留）
    "translator_create_capture",
    "translator_free_capture",
    "translator_start_capture",
    "translator_stop_capture",
    "translator_push_chunk",
    "translator_poll_chunk",
    "translator_dropped_frames",
    "translator_probe_capture",
    // 其它
    "translator_version",
    "translator_is_offline",
    "translator_free_string",
    "translator_capability_desc",
    "translator_chunk_samples",
    "translator_sample_rate",
    "translator_discover_models",
    "translator_probe_system_load",
    "translator_resource_tiers",
];

/// 找到构建产物里的 `translator_core.dll`
fn find_dll() -> Option<PathBuf> {
    // 1) 优先用环境变量（打包脚本会指到 staging）
    if let Ok(p) = std::env::var("TRANSLATOR_DLL") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // 2) core/target/release/
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("release")
        .join("translator_core.dll");
    if local.is_file() {
        return Some(local);
    }
    // 3) 仓库根的 outputs/
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("outputs").join("translator_core.dll"));
    repo.filter(|p| p.is_file())
}

/// 从 DLL 二进制里提取导出的符号名。
///
/// Windows PE 的导出表里，函数名以明文出现。
/// 这里用「扫 `translator_` 前缀的连续可打印字符」的方式提取，
/// 比解析 PE 结构简单且足够可靠（这些符号不会以非 ASCII 出现）。
fn extract_exports(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let name = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
            if name.starts_with("translator_") {
                out.push(name.to_string());
            }
        } else {
            i += 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn all_required_exports_are_present_in_dll() {
    let dll = match find_dll() {
        Some(p) => p,
        None => {
            eprintln!("[SKIP] 未找到 translator_core.dll，跳过导出完整性检查");
            return;
        }
    };
    let bytes = std::fs::read(&dll).expect("读 DLL 失败");
    let exports = extract_exports(&bytes);

    eprintln!("DLL: {}", dll.display());
    eprintln!("导出符号数: {}", exports.len());

    let missing: Vec<&&str> = REQUIRED_EXPORTS
        .iter()
        .filter(|name| !exports.iter().any(|e| e == *name))
        .collect();

    if !missing.is_empty() {
        eprintln!("导出的 translator_* 符号：");
        for e in &exports {
            eprintln!("  {e}");
        }
    }

    assert!(
        missing.is_empty(),
        "DLL 缺少以下导出符号（Dart 侧 lookupFunction 会直接失败 → 应用起不来）: {:?}",
        missing
    );
}
