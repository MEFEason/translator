//! 构建脚本：`mt-bergamot` feature 启用时，用 CMake 编译 Bergamot 静态库。
//!
//! ## 为什么需要它
//!
//! Bergamot 的 Rust 绑定不存在，社区也不提供预编译产物。
//! 必须自行编译 `bergamot-translator` 的 C++ 源码为静态库。
//!
//! ## 为什么这满足「完全离线」
//!
//! - **不下载任何东西**：源码来自本地 `third_party/bergamot-translator`
//!   （git submodule，需在构建机一次性 clone）
//! - **不引入 bindgen**：手写 FFI 声明在 `mt/bergamot.rs`
//! - 产物 `libbergamot-translator.a` 静态链接进最终二进制
//!
//! 与 CTranslate2 的关键差异：后者 `ctranslate2-sys` 在 build.rs 里
//! **联网下载**预编译包（实测代理 502 失败），违反离线性约束。
//!
//! ## 环境变量
//!
//! - `BERGAMOT_SRC_DIR` —— 直接指定源码目录，跳过 submodule 查找
//! - `BERGAMOT_PREBUILT` —— 指向已编译的静态库，跳过 CMake 构建
//! - `MEFEASON_BUILD_NATIVE=0` —— 跳过原生构建（纯 Rust 交叉编译时用）

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// CMake 构建阶段记下的 build 根目录，等包装层声明完再输出链接指令。
static PENDING_BUILD_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 生成版本串。
///
/// 格式：`{channel}-v{semver}-build{YYYY}.{MMDD}.{HH}.{mm}`
/// 例如 `beta-v0.9.1-build2026.1022.15.16`（测试版）
///      `release-v1.0.0-build2026.1022.15.16`（正式版）
///
/// ## 四段各是什么（对应版本号规范 A.B.C.D + 渠道）
///
/// 参照通用版本号规范（主版本号.次版本号.修订号，加阶段标识符与构建号）：
///
/// - `channel` —— 渠道（snapshot/alpha/beta/pre/release/stable/current），
///   标识这个构建处于什么阶段。
/// - `v{semver}` —— 语义版本 `v{主}.{次}.{修订}`（如 `v0.9.1`），
///   由 `TRANSLATOR_SEMVER` 注入。
/// - `build{time}` —— 构建号 `build{YYYY}.{MMDD}.{HH}.{mm}`（如
///   `build2026.1022.15.16`），标识这是第几次构建。
///
/// ## 语义版本的递进规则（用户规定）
///
/// - **正式版大迭代** → 更新全部数字：`v1.0.0`、`v2.0.0`
/// - **测试版** → 只改后面的数字，前面依旧为 0：
///   `0.9.1` → `0.9.8` → `0.9.12` → `0.9.45` → `0.10.56`
/// - 上一级有变动，下级要归零（规范通用准则）。
///
/// 年份用 **4 位**（用户要求）：2 位年有歧义（26 → 2026 还是 1926）。
///
/// ## 渠道（沿用 Maven 的版本语义）
///
/// | 渠道 | 含义 | 建议 |
/// |---|---|---|
/// | `snapshot` | 开发版。功能会不时更新导致变化，正式环境不得使用 | 不建议 |
/// | `alpha` | 内部测试版。给开发与测试人员测试找 BUG | 不建议 |
/// | `beta` | 公开测试版。比 alpha 进一步，面向公众测试 | 不建议 |
/// | `pre` | 预发布版，类似 alpha，有时细分 M1/M2 | 不建议 |
/// | `release` | 发行版。官方推荐使用的版本 | 推荐 |
/// | `stable` | 稳定版。比测试版更稳定，去除了许多缺陷 | 推荐 |
/// | `current` | 最新版，但不一定是稳定版本，可能存在隐患 | 技术过硬者使用 |
///
/// ## 本地时间从哪来
///
/// `std::time` 只有 UTC，而版本号里的时间应该是**本地时间**（用户在
/// 自己的机器上构建，看到的应是自己那边的时间）。
/// 故由构建脚本注入 `TRANSLATOR_BUILD_TIME`（`2026-10-22 15:16:30`，
/// 本地时间），本函数只做格式化。缺失时退回 UTC。
fn emit_version() {
    println!("cargo:rerun-if-env-changed=TRANSLATOR_CHANNEL");
    println!("cargo:rerun-if-env-changed=TRANSLATOR_SEMVER");
    println!("cargo:rerun-if-env-changed=TRANSLATOR_BUILD_TIME");

    // 渠道：白名单内才采纳，非法值退回 snapshot（当前是开发版）
    const CHANNELS: [&str; 7] = [
        "snapshot", "alpha", "beta", "pre", "release", "stable", "current",
    ];
    // 注意：不能写 `std::env::var(..).as_deref()` ——
    // Result 的临时值在 match 结束前就被释放，编译器会拒绝（E0716）。
    let channel_env = std::env::var("TRANSLATOR_CHANNEL").ok();
    let channel = match channel_env.as_deref() {
        Some(c) if CHANNELS.contains(&c) => c,
        Some(other) => {
            println!(
                "cargo:warning=未知渠道 TRANSLATOR_CHANNEL={other}，退回 snapshot"
            );
            "snapshot"
        }
        None => "snapshot",
    };

    // 语义版本：形如 `0.9.1` / `1.0.0`（不带 v 前缀，拼串时补）。
    // 校验为「三段非空纯数字」；不合法时退回 0.0.0 并告警 ——
    // 版本号写错比退回默认更危险（用户会拿到看不出问题的包）。
    let semver = match std::env::var("TRANSLATOR_SEMVER").ok().as_deref() {
        Some(s) if is_valid_semver(s) => s.to_string(),
        Some(other) => {
            println!(
                "cargo:warning=TRANSLATOR_SEMVER={other} 不是 X.Y.Z 三段数字，退回 0.0.0"
            );
            "0.0.0".to_string()
        }
        None => {
            println!("cargo:warning=未设置 TRANSLATOR_SEMVER，退回 0.0.0");
            "0.0.0".to_string()
        }
    };

    // 时间：优先用注入的本地时间，缺失则用 UTC
    let (yyyy, mm, dd, hh, mi) = match std::env::var("TRANSLATOR_BUILD_TIME") {
        Ok(s) => parse_time(&s).unwrap_or_else(utc_now),
        Err(_) => utc_now(),
    };

    // 版本号用 **4 位年**（用户要求）：`2026.1022.15.16`
    // 2 位年有歧义（26 → 2026 还是 1926），且跨世纪后无法区分。
    let time = format!("{yyyy}.{mm:02}{dd:02}.{hh:02}.{mi:02}");
    let build = format!("build{time}");
    let full = format!("{channel}-v{semver}-{build}");

    println!("cargo:rustc-env=TRANSLATOR_BUILD_VERSION={full}");
    // 仅语义版本（不带渠道与构建号），供 UI / 脚本单独取用
    println!("cargo:rustc-env=TRANSLATOR_BUILD_SEMVER=v{semver}");
    println!("cargo:rustc-env=TRANSLATOR_BUILD_VERSION_SHORT=v{semver}");
    // 构建号单独导出（build2026.1022.15.16）
    println!("cargo:rustc-env=TRANSLATOR_BUILD_BUILD={build}");
    println!("cargo:rustc-env=TRANSLATOR_BUILD_TIME_COMPACT={time}");
    println!("cargo:rustc-env=TRANSLATOR_BUILD_STAMP={yyyy}.{mm:02}.{dd:02} {hh:02}:{mi:02}");
    println!("cargo:rustc-env=TRANSLATOR_BUILD_CHANNEL={channel}");
}

/// 语义版本是否合法：`X.Y.Z`，三段且每段都是非空纯数字。
///
/// 不接受 `v` 前缀、不接受预发布后缀（`-rc1`）—— 那些信息由渠道承载。
fn is_valid_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// 解析 `YYYY-MM-DD HH:MM:SS` → (yyyy, mm, dd, hh, mi)
fn parse_time(s: &str) -> Option<(u32, u32, u32, u32, u32)> {
    let s = s.trim();
    let (date, time) = s.split_once(' ')?;
    let mut dp = date.split('-');
    let y: u32 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    let mut tp = time.split(':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
        return None;
    }
    Some((y, mo, d, h, mi))
}

/// UTC 兜底（无本地时间注入时）
fn utc_now() -> (u32, u32, u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (
        y as u32,
        mo,
        d,
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
    )
}

fn main() {
    emit_version();
    println!("cargo:rerun-if-env-changed=BERGAMOT_SRC_DIR");
    println!("cargo:rerun-if-env-changed=BERGAMOT_PREBUILT");
    println!("cargo:rerun-if-env-changed=MEFEASON_BUILD_NATIVE");
    println!("cargo:rerun-if-env-changed=CMAKE");
    println!("cargo:rerun-if-changed=csrc/bergamot_c_api.cpp");

    let _target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // ★★ sherpa-onnx-c-api.dll 必须 **delay-load** ★★
    //
    // 原因链：
    // 1. `sherpa-onnx-c-api.dll` 静态导入 `onnxruntime.dll`（非 delay）
    // 2. 若不延迟，Windows 加载器在**进程启动解析 import** 时就去找 ORT
    // 3. 搜索顺序里 System32 优先于 PATH → 命中系统自带的 1.17
    // 4. sherpa 1.13.6 需要 API 27（ORT 1.27+）→ 加载即崩
    //
    // 延迟后：sherpa 只在**首次调用**时才加载 → 那时应用已完成
    // `ort_bootstrap::ensure_ort_ready()`，exe 同目录已有正确的 1.27。
    //
    // MSVC 用 /DELAYLOAD:；其它工具链不支持，只能靠系统里恰好有对的 ORT。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // ★ ssentencepiece_core 必须**强制抽取**。
        //
        // 原因：MSVC 链接器是**单遍扫描** —— 静态库只有在存在未决符号时
        // 才抽取其中的 obj。crate 把 ssentencepiece_core 作为 native 库
        // 放在 rlib **之前**，扫到它时还没有任何未决符号（用到
        // sentencepiece 符号的 obj 在后面的 rlib / marian.lib 里）
        // → 它整个被跳过 → marian.lib 报 LNK2001
        // 「无法解析的外部符号 sentencepiece::SentencePieceProcessor」。
        //
        // `+whole-archive` 让链接器把库内 obj 全部拉进来。
        // 注意：MSVC 的 `/WHOLEARCHIVE:` **不接受完整路径**（只认库名），
        // 必须用 rustc-link-lib 的 `+whole-archive` 前缀 + link-search。
        if let Ok(dir) = std::env::var("SHERPA_ONNX_LIB_DIR") {
            let sp = PathBuf::from(&dir).join("ssentencepiece_core.lib");
            if sp.exists() {
                println!("cargo:rustc-link-search=native={}", dir);
                println!("cargo:rustc-link-lib=static:+whole-archive=ssentencepiece_core");
            }
        }
    }

    // 只在启用 mt-bergamot 时编译原生库
    if std::env::var("CARGO_FEATURE_MT_BERGAMOT").is_err() {
        return;
    }

    // 纯 Rust 交叉编译（如 Android NDK 构建的 host 侧）可跳过
    if std::env::var("MEFEASON_BUILD_NATIVE").as_deref() == Ok("0") {
        println!("cargo:warning=MEFEASON_BUILD_NATIVE=0，跳过 Bergamot 原生构建");
        return;
    }

    // 已提供预编译库 → 直接链接
    if let Ok(prebuilt) = std::env::var("BERGAMOT_PREBUILT") {
        let p = PathBuf::from(&prebuilt);
        if p.exists() {
            println!("cargo:rustc-link-search=native={}", p.display());
            // 预编译库所在目录的上级通常是 CMake build 根，
            // 从那里读 build.ninja 拿完整依赖清单。
            // 逐级上溯，最多找 4 层（…/build/src/translator → …/build）。
            let mut build_root = p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| p.clone());
            for _ in 0..4 {
                if build_root.join("build.ninja").exists() {
                    break;
                }
                match build_root.parent() {
                    Some(up) => build_root = up.to_path_buf(),
                    None => break,
                }
            }
            // ★ 顺序很重要：包装层必须在主库**之前**声明。
            // 链接命令行里 native lib 按声明顺序排列，包装层在前，
            // 它对主库的符号引用才能在扫主库时进未决表。
            if let Some(src) = locate_source() {
                build_c_api(&src);
            }
            link_all(&build_root);
            return;
        }
        println!(
            "cargo:warning=BERGAMOT_PREBUILT 指向 {} 但不存在，回退到源码构建",
            p.display()
        );
    }

    let src_dir = locate_source();
    let Some(src) = src_dir else {
        // 不 panic：让链接期报错，错误信息更明确地指向缺失原生库
        println!(
            "cargo:warning=Bergamot 源码未找到。链接会失败。\n\
             请执行其一：\n\
             \x20 (1) git submodule update --init --recursive\n\
             \x20 (2) 设置 BERGAMOT_SRC_DIR 指向 bergamot-translator 源码目录\n\
             \x20 (3) 设置 BERGAMOT_PREBUILT 指向已编译的 libbergamot-translator.a"
        );
        return;
    };

    println!("cargo:rerun-if-changed={}", src.display());
    build_with_cmake(&src);
    // 顺序：包装层先声明（必须在主库之前），再输出主库及其依赖的 link 指令
    build_c_api(&src);
    link_all_pending();
}

/// 编译 Bergamot 的 C ABI 包装层。
///
/// Bergamot 只有 C++ 接口，Rust 无法直接 FFI 调用（虚表/模板）。
/// 这一层把 `create/destroy/translate/translate_multi/free_string`
/// 暴露成稳定 C ABI，Rust 侧用 `extern "C"` 声明。
///
/// 用 `cc` crate 而非 CMake：包装层只有 1 个文件，
/// 走 CMake 反而多一层生成器依赖。
fn build_c_api(src: &Path) {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let c_api = manifest.join("csrc/bergamot_c_api.cpp");
    if !c_api.exists() {
        println!("cargo:warning=C 包装层不存在，跳过：{}", c_api.display());
        return;
    }

    // Bergamot 的公开头文件在 src/translator/ 下，其内部头用
    // 「3rd_party/marian-dev/src/3rd_party/CLI/CLI.hpp」这种**相对仓库根**的
    // 路径去 include —— 所以必须把仓库根本身也加进 include 路径，
    // 否则 parser.h 的第一个 include 就找不到。
    let mut build = cc::Build::new();
    build.cpp(true).file(&c_api);
    // Bergamot 要求 C++17（marian-dev 用 structured binding / if-init）
    build.std("c++17");
    build.flag_if_supported("-Wno-unused-parameter");
    // MSVC 下 Bergamot 头文件带 GCC 专有 pragma → C4068 无害警告
    build.flag_if_supported("/wd4068");
    // ★ CRT 必须与 Bergamot 全树、sherpa-onnx 预编译库一致：**/MT**。
    // 原因链：sherpa-onnx 官方 Windows 预编译库是 static-MT
    // （目录名 sherpa-onnx-v1.13.6-win-x64-static-MT-Release-lib），
    // 改不动 → Bergamot 全树（marian/sentencepiece/pcre2/app）必须统一 /MT
    // → 我们的包装层也用 /MT。
    // Rust 侧由 .cargo/config.toml 的 `crt-static`（-C target-feature=+crt-static）
    // 对齐，否则链接期 LNK2038「RuntimeLibrary 不匹配」。
    // cc crate 默认 /MD → 必须显式改 /MT。
    if cfg!(windows) && cfg!(target_env = "msvc") {
        build.flag("/MT");
    }
    build.include(src); // 仓库根：解析 "3rd_party/marian-dev/src/..." 形式
    build.include(src.join("src"));
    build.include(src.join("3rd_party/marian-dev/src"));
    build.include(src.join("3rd_party/marian-dev/src/3rd_party"));
    // text_processor.h 用 `#include "ssplit.h"`（不带目录）→ 必须给到
    // ssplit 头文件所在目录本身，而不是它的父目录。
    build.include(src.join("3rd_party/ssplit-cpp/src/ssplit"));
    // 抑制 cc crate 自己输出 link 指令 —— 我们要自己输出，
    // 因为必须带 `+whole-archive` 修饰符。
    build.cargo_metadata(false);
    build.compile("bergamot_c_api");

    // ★ 包装层必须用 whole-archive，且**声明在主库之前**。
    //
    // 死锁式的顺序问题：
    // 1. Rust 把 rlib 排在所有 native static lib **之后**。
    // 2. 包装层在 rlib 里（cc crate 生成的 .lib 也在 native 列表里，
    //    但它的 obj 只有在「被引用」时才会被链接器拉入）。
    // 3. 引用包装层的是 Rust 代码（在后面的 rlib 里）→ 扫到包装层 .lib
    //    时它还没被拉入 → 包装层对主库的引用不进未决表
    //    → 扫主库时主库 obj 不被拉入 → LNK2019。
    //
    // 解法：包装层用 whole-archive 强制拉入（它很小，代价可忽略），
    // 并让它排在主库之前 → 包装层的引用先进未决表 → 主库 obj 被拉入。
    //
    // ⚠️ 不要对**主库**用 whole-archive：那会把主库里大量用不到的 obj
    // 强拉进来，它们引用 marian 内部符号而 marian 的 obj 又没被拉入
    // → 一次冒出 75 个 LNK2001。
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=bergamot_c_api");

    println!("cargo:warning=Bergamot C ABI 包装层已编译（whole-archive）");
}

fn link_static() {
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => {
            // MSVC 用静态库 lib
            println!("cargo:rustc-link-lib=static=bergamot-translator");
        }
        Ok("macos") | Ok("ios") => {
            println!("cargo:rustc-link-lib=static=bergamot-translator");
        }
        _ => {
            println!("cargo:rustc-link-lib=static=bergamot-translator");
        }
    }
}

/// 查找 Bergamot 源码
fn locate_source() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BERGAMOT_SRC_DIR") {
        let p = PathBuf::from(dir);
        if p.join("CMakeLists.txt").exists() {
            return Some(p);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest.join("../third_party/bergamot-translator"),
        manifest.join("../../third_party/bergamot-translator"),
    ];
    candidates.into_iter().find(|p| p.join("CMakeLists.txt").exists())
}

fn cmake_bin() -> PathBuf {
    if let Ok(c) = std::env::var("CMAKE") {
        return PathBuf::from(c);
    }
    // Windows 下 VS 自带 CMake，但不在 PATH
    if cfg!(windows) {
        let vs_cmake = PathBuf::from(
            r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe",
        );
        if vs_cmake.exists() {
            return vs_cmake;
        }
        let vs18 = PathBuf::from(
            r"C:\Program Files\Microsoft Visual Studio\18\Professional\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe",
        );
        if vs18.exists() {
            return vs18;
        }
    }
    PathBuf::from("cmake")
}

fn build_with_cmake(src: &Path) {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let build_dir = out_dir.join("bergamot-build");
    let cmake = cmake_bin();

    // 关闭不需要的组件：单元测试、Python 绑定、WASM
    let args = vec![
        "-S".to_string(),
        src.display().to_string(),
        "-B".to_string(),
        build_dir.display().to_string(),
        "-DCMAKE_BUILD_TYPE=Release".to_string(),
        "-DCOMPILE_UNIT_TESTS=OFF".to_string(),
        "-DCOMPILE_TESTS=OFF".to_string(),
        "-DCOMPILE_PYTHON=OFF".to_string(),
        "-DCOMPILE_WASM=OFF".to_string(),
    ];

    println!("cargo:warning=编译 Bergamot（首次较慢，仅需一次）…");
    if !run(&cmake, &args) {
        panic!(
            "Bergamot CMake 配置失败。请确认已安装 CMake 与 C++17 编译器，\
             且已 clone bergamot-translator 源码（git submodule update --init --recursive）"
        );
    }

    if !run(
        &cmake,
        &[
            "--build".to_string(),
            build_dir.display().to_string(),
            "--config".to_string(),
            "Release".to_string(),
        ],
    ) {
        panic!("Bergamot 编译失败");
    }

    // 产物路径各平台不同
    let lib_dir = if cfg!(windows) {
        build_dir.join("Release")
    } else {
        build_dir.clone()
    };
    let candidates = [
        lib_dir.join("bergamot-translator.lib"),
        lib_dir.join("libbergamot-translator.a"),
        lib_dir.join("libbergamot-translator.dylib"),
        build_dir.join("libbergamot-translator.a"),
    ];

    if let Some(found) = candidates.iter().find(|p| p.exists()) {
        if let Some(dir) = found.parent() {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
        // 注意：这里**不**调 link_all —— 链接顺序由调用方控制
        // （包装层必须在主库之前声明，见 main()）。
        link_all_later(&build_dir);
    } else {
        println!(
            "cargo:warning=Bergamot 构建完成但未找到静态库，搜索目录: {}",
            build_dir.display()
        );
    }
}

/// 延迟链接：把 build_dir 记下来，由 main 在正确时机调用 link_all。
///
/// 链接顺序有硬约束（包装层必须在主库之前），所以 CMake 构建阶段
/// 不能直接输出主库的 link 指令 —— 得等包装层声明完再输出。
fn link_all_later(build_dir: &Path) {
    let _ = PENDING_BUILD_DIR.set(build_dir.to_path_buf());
}

/// 输出延迟的链接指令（若 CMake 构建阶段记下了 build_dir）。
fn link_all_pending() {
    if let Some(p) = PENDING_BUILD_DIR.get() {
        link_all(p);
    }
}

/// 链接 Bergamot 及其全部依赖库。
///
/// ## 为什么不能只链一个 `bergamot-translator`
///
/// Bergamot 的静态库只含 `src/translator/` 下的代码，
/// 依赖 marian、yaml-cpp、phf、sentencepiece、intgemm、ruy（拆成 30+ 个
/// 接口库）、ssplit、pcre2 —— 全是**独立 .lib**。
/// 只链 `bergamot-translator` 会在链接期爆一屏 LNK2001。
///
/// ## 库清单从哪来
///
/// 从 CMake 生成的 `build.ninja` 里读 `app/bergamot.exe` 目标的
/// `LINK_LIBRARIES` —— 那是**权威来源**：CMake 换了 ruy 版本、
/// 加了新依赖，这个清单自动跟着变，不需要在这里硬编码几十个库名。
///
/// 解析不出时退回「只链主库」并给出警告（至少让错误信息指向链接期
/// 而不是构建期）。
fn link_all(build_dir: &Path) {
    let ninja = build_dir.join("build.ninja");
    let Some(text) = std::fs::read_to_string(&ninja).ok() else {
        println!("cargo:warning=读不到 build.ninja，只链接 bergamot-translator");
        link_static();
        return;
    };

    // 找 **可执行目标**（app/bergamot.exe）的 LINK_LIBRARIES 行。
    // 注意：build.ninja 里有成百上千条 LINK_LIBRARIES（每个库目标一条），
    // 必须按目标名精确定位，否则会拿到某个中间库的清单，漏掉主库。
    let mut libs: Vec<String> = Vec::new();
    let mut in_target = false;
    for line in text.lines() {
        if line.starts_with("build ") {
            // 新目标块开始
            in_target = line.contains("bergamot.exe") || line.contains("service-cli");
            continue;
        }
        if !in_target || !line.trim_start().starts_with("LINK_LIBRARIES") {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let rhs = &line[eq + 1..];
        for tok in rhs.split_whitespace() {
            // 系统库：Rust 链接器默认带上大部分 Windows 系统库，
            // 但 **SHLWAPI 不在默认集里** —— marian 的 path.cpp 用了
            // PathMatchSpecW，漏了它会 LNK2001「无法解析的外部符号
            // __imp_PathMatchSpecW」。所以必须保留。
            const SYSTEM_LIBS: &[&str] = &[
                "kernel32", "user32", "gdi32", "winspool", "shell32", "ole32", "oleaut32", "uuid",
                "comdlg32", "advapi32",
            ];
            let stem = tok.trim_end_matches(".lib");
            if SYSTEM_LIBS.contains(&stem) {
                continue;
            }
            if !tok.ends_with(".lib") && !tok.ends_with(".a") {
                continue;
            }
            libs.push(tok.to_string());
        }
        break;
    }

    if libs.is_empty() {
        println!("cargo:warning=未能从 build.ninja 解析出链接库清单");
        link_static();
        return;
    }

    // 每个库所在目录都要加进搜索路径
    let mut emitted: Vec<String> = Vec::new();
    for lib in &libs {
        let p = build_dir.join(lib);
        let lib_dir = p.parent().map(|d| d.to_path_buf());
        if let Some(dir) = &lib_dir {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
        // ★ SHLWAPI 是 Windows **系统 import lib**（shlwapi.lib 在 SDK 里，
        // 不在 build_dir 下），没有静态库。用完整路径会找不到，
        // 必须用普通（dll）链接形式。
        if lib.eq_ignore_ascii_case("SHLWAPI.lib") {
            println!("cargo:rustc-link-lib=shlwapi");
            continue;
        }
        // ★★ 不链 sentencepiece.lib —— 让 sherpa 自带的 ssentencepiece_core 提供 ★★
        //
        // 原因：sherpa-onnx 的 static 库里带了一份完整的 sentencepiece +
        // protobuf-lite（`ssentencepiece_core.lib`），与 Bergamot 依赖树里
        // sentencepiece.lib 的那份**符号完全相同**
        // （都是 google::protobuf::internal::*）→ LNK2005「已经在
        // libsherpa_onnx_sys.rlib 中定义」。
        //
        // 用 /FORCE:MULTIPLE 绕开重复定义检查虽然能链接通过，但运行时会崩：
        // 链接器可能选中 sherpa 那份**精简** protobuf（缺 Arena/ExtensionSet），
        // 而 sentencepiece 读 .spm 词表需要完整版 →
        // 「fatal runtime error: Rust cannot catch foreign exceptions」
        // （ORT 初始化时 C++ 异常穿过 Rust 帧）。
        //
        // 干脆不链 sentencepiece.lib：sherpa 的 ssentencepiece_core
        // 提供同名符号，链接器按需抽取它，**零冲突**，也不用 /FORCE:MULTIPLE。
        // sentencepiece 照常链接：它的 protobuf 已被 patch_protobuf_ns.py
        // 隔离到 `spm_google` 命名空间 → 与 sherpa 自带的那份
        // （`google::protobuf`）符号不再重名 → 无需任何链接顺序技巧。
        //
        // 历史上曾试过「不链 sentencepiece.lib，让 sherpa 的
        // ssentencepiece_core 顶上」—— 行不通：sherpa 把 sentencepiece
        // 编进了自己的命名空间（符号是 `sentencepiece::Ssentencepiece`），
        // 与 marian 需要的 `sentencepiece::SentencePieceProcessor`
        // **不是同一个符号**，顶不上。
        // ★ 直接传完整路径，不用 rustc-link-lib=static=name。
        //
        // 原因：rustc-link-lib=static 对 MSVC 会转成 `/DEFAULTLIB:name.lib`。
        // /DEFAULTLIB 与 /WHOLEARCHIVE 组合时，whole-archive 拉入的
        // 包装层 obj 产生的未决符号不会被后续 /DEFAULTLIB 补齐
        // （实测 LNK2019：parseOptionsFromFilePath 等 6 个符号全丢）。
        // 而 CMake 的链接方式是**直接把 .lib 路径列在命令行上**，
        // link.exe 单遍扫描按顺序解析，行为完全可预期。
        // 直接传路径同时避开了库名推导的坑（连字符、lib 前缀等）。
        if p.exists() {
            println!("cargo:rustc-link-arg={}", p.display());
            continue;
        }
        // 文件不在预期位置 → 回退按名链接（搜索路径已加）
        let file = Path::new(lib);
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = if cfg!(windows) {
            stem
        } else {
            stem.strip_prefix("lib").unwrap_or(&stem).to_string()
        };
        if emitted.contains(&name) {
            continue;
        }
        emitted.push(name.clone());
        println!("cargo:rustc-link-lib=static={name}");
    }
    // ★ 只扫一遍。
    //
    // 曾试过「把库清单输出两遍」来应付 MSVC link.exe 的单遍扫描
    // （marian.lib 引用 sentencepiece_train.lib 里的 Train 符号），
    // 但那个问题真正的解法是**移除 Rust 的 sentencepiece crate**
    // —— 它自己编译了一份同名 sentencepiece_train.lib，与 marian 的那份
    // 符号冲突。两遍扫描会让 link.exe 把 37 个库（合计 250MB+）全部
    // 处理两次，内存占用直接翻倍（用户反馈 link.exe 吃内存）。
    //
    // 教训：**先消除冲突源，别用重复链接去绕。**
    println!("cargo:warning=已链接 {} 个 Bergamot 依赖库", emitted.len());

}

fn run(cmd: &Path, args: &[String]) -> bool {
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
