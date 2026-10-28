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
/// 格式：`{platform}-{channel}-v{semver}-{YYYY}.{MMDD}.{HH}.{mm}`
/// 例如 `windows-release-v1.0.0-2026.1028.09.35`（正式版 · Windows 端）
///      `android-beta-v0.9.7-2026.1028.09.35`（测试版 · Android 端）
///
/// ## 端标识大小写（用户规定，2026-10-28）
///
/// 端标识**全小写**（`android` / `windows`），与用户给的样例
/// `android-test-v0.9.7-2026.1028.09.35` 对齐（样例里的 test 是占位，
/// 实际渠道取白名单值，如 beta）。
///
/// ## 五段各是什么（对应版本号规范 A.B.C.D + 阶段标识符 + 构建号）
///
/// - `platform` —— **端标识**（windows / android），标识这个包属于哪一端。
///   由 `TRANSLATOR_PLATFORM` 注入；同一仓库要出两端的包，靠它区分。
/// - `channel` —— 渠道（snapshot/alpha/beta/pre/release/stable/current），
///   标识这个构建处于什么阶段。
/// - `v{semver}` —— 语义版本 `v{主}.{次}.{修订}`（如 `v0.9.1`），
///   由 `TRANSLATOR_SEMVER` 注入。
/// - `{YYYY}.{MMDD}.{HH}.{mm}` —— 构建时间戳（本地时间），
///   标识这是第几次构建。
///
/// ## 端标识位置（用户规定，2026-10-28）
///
/// 端标识放在**渠道前面**：`windows-release-…` / `android-release-…`。
/// 之前版本号不带端标识，Windows 包与 Android 包版本串完全一样，
/// 混在同一批产物里时无法分辨来源。
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
    println!("cargo:rerun-if-env-changed=TRANSLATOR_PLATFORM");
    println!("cargo:rerun-if-env-changed=TRANSLATOR_CHANNEL");
    println!("cargo:rerun-if-env-changed=TRANSLATOR_SEMVER");
    println!("cargo:rerun-if-env-changed=TRANSLATOR_BUILD_TIME");

    // 端标识：只有 windows / android 两个合法值（**全小写**，
    // 用户 2026-10-28 规定；Windows 端会话同步时注意此变更）。
    // 优先级：
    //   1. TRANSLATOR_PLATFORM 显式注入（构建脚本指定，大小写不敏感）；
    //   2. 按编译目标自动推断（CARGO_CFG_TARGET_OS）——
    //      cargo 交叉编译 Android 时该变量是 "android"，Windows 是 "windows"；
    //   3. 都没有 → 退回 windows（当前唯一有安装器的一端）。
    //
    // 自动推断这一层很重要：Android 端构建脚本若忘了注入平台标识，
    // 版本串会静默变成 `windows-…`，把 Android 包说成 Windows 包。
    const PLATFORMS: [&str; 2] = ["windows", "android"];
    let platform_env = std::env::var("TRANSLATOR_PLATFORM")
        .ok()
        .map(|s| s.to_lowercase());
    let platform = match platform_env.as_deref() {
        Some(p) if PLATFORMS.contains(&p) => p,
        Some(other) => {
            println!(
                "cargo:warning=未知端标识 TRANSLATOR_PLATFORM={other}，退回 windows"
            );
            "windows"
        }
        None => match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
            Ok("android") => "android",
            Ok("windows") => "windows",
            _ => "windows",
        },
    };

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
    // 版本串：{端标识}-{渠道}-v{语义版本}-{时间戳}
    // 例：windows-release-v1.0.0-2026.1028.09.35
    let full = format!("{platform}-{channel}-v{semver}-{time}");

    println!("cargo:rustc-env=TRANSLATOR_BUILD_VERSION={full}");
    // 仅语义版本（不带渠道与构建号），供 UI / 脚本单独取用
    println!("cargo:rustc-env=TRANSLATOR_BUILD_SEMVER=v{semver}");
    println!("cargo:rustc-env=TRANSLATOR_BUILD_VERSION_SHORT=v{semver}");
    // 端标识单独导出（windows / android）
    println!("cargo:rustc-env=TRANSLATOR_BUILD_PLATFORM={platform}");
    // 构建号单独导出（时间戳 2026.1022.15.16）
    println!("cargo:rustc-env=TRANSLATOR_BUILD_BUILD={time}");
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

    // ★ Android cdylib 必须显式带 SONAME（2026-10-28 真机闪退教训）。
    //
    // 现象：libnative_bridge.so 的 DT_NEEDED 里记的不是 "libtranslator_core.so"
    // 而是**构建机的完整路径**（C:/Users/.../jniLibs/arm64-v8a/libtranslator_core.so）
    // —— 设备上 dlopen 必失败：
    //   "dlopen failed: library "C:/Users/..." not found: needed by base.apk!/lib/.../libnative_bridge.so"
    //
    // 根因：Rust 的 cdylib 在 Linux/Android 工具链下**默认不写 SONAME**，
    // CMake 链接 IMPORTED SHARED 时，没有 SONAME 就把文件路径原样记进 NEEDED。
    // 修复：链接时补 -soname，让产物声明自己的逻辑名；
    // 之后 CMake 链接会记 "libtranslator_core.so"，运行时在 APK lib 目录解析成功。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-arg=-Wl,-soname,libtranslator_core.so");
    }

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

    // ★★★ Android（arm64）交叉编译路径（2026-10-28）★★★
    //
    // 与桌面端（MSVC / Windows）**完全不同的机制**：
    //
    // - 桌面端：BERGAMOT_PREBUILT 指向 MSVC 静态库目录，
    //   从 build.ninja 读 `app/bergamot.exe` 的 LINK_LIBRARIES 拿全量清单；
    // - Android 端：交叉编译产物在 `build-android/`，且我们用
    //   `COMPILE_LIBRARY_ONLY=ON`（只要库，不要 app 可执行文件）
    //   → **没有 bergamot.exe 目标可读**。
    //
    // 故 Android 走「清单文件」路线：交叉编译脚本
    // （Android/scripts/build_bergamot_android.sh）用
    // `gen_bergamot_libs.py` 从 build.ninja 递归解析依赖图，
    // 导出 `bergamot_libs.txt`（每行一个 .a 的绝对路径，
    // 顺序 = CMake 的拓扑序 = 正确的静态链接顺序）。
    // build.rs 读它并翻译成 rustc-link-* 指令。
    //
    // ⚠️ 为什么不合并成一个 .a：llvm-ar 的 MRI 模式在 Windows 上
    //    对含非 ASCII 字符的路径**静默失败**（rc=0、无输出文件），
    //    本仓库路径正好命中。绕开比硬啃更划算。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        link_android();
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
    //
    // ⚠️⚠️ 这里**必须用 CARGO_CFG_TARGET_* 环境变量判断，不能用 `cfg!()`**：
    //   build.rs 是给 **host** 编译的（`cfg!(windows)` 求值的是构建机，
    //   在本机恒为 true！）→ 交叉编译 Android 时会给 NDK clang 传 `/MT`，
    //   clang 不认 MSVC 的 /MT → "clang++: error: no such file or
    //   directory: '/MT'"，整个构建失败（2026-10-28 实测）。
    //
    //   正确写法：读 Cargo 注入的 target 配置环境变量。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        build.flag("/MT");
    }

    // ★ ARM 目标必须补 `ARM FMA SSE` 三个宏（2026-10-28）。
    //
    // 原因链（本项目实测踩过）：
    // 1. marian 的 `functional/operators.h` 按**编译器宏** `__ARM_NEON`
    //    判断走 ARM 分支 → include `simd_utils/simd_utils.h`；
    // 2. 但 `simd_utils_constants.h` 内部是按 **`-DSSE` 宏**决定要不要
    //    include `sse2neon_wrapper.h`（x86 SSE → ARM NEON 转译层）；
    // 3. 缺 -DSSE → 不 include → `_mm_add_ps` / `_mm_mul_ps` /
    //    `sin_ps` 等全部未声明 → 编译报"use of undeclared identifier"。
    //
    // 为什么 x86 上不需要：那时 `operators.h` 走 `#include <immintrin.h>`
    //（编译器原生 SSE），压根不碰 sse2neon。
    //
    // 为什么 CMake 里已经设过了还要在这里设：CMake 的
    // `add_compile_definitions(ARM FMA SSE)` 只作用于 **marian 目录作用域**，
    // 而本包装层是 Rust 侧独立编译的（cc crate），拿不到那些宏。
    if target_os == "android" || target_os == "linux" {
        let is_arm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
            || std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm");
        if is_arm {
            build.define("ARM", None);
            build.define("FMA", None);
            build.define("SSE", None);
        }
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

/// Android（arm64）交叉编译的链接路径（2026-10-28）。
///
/// ## 流程
///
/// 1. 读 `bergamot_libs.txt`（由 `Android/scripts/gen_bergamot_libs.py`
///    从 CMake 的 `build.ninja` 递归解析依赖图导出）；
/// 2. 编译 C ABI 包装层（`csrc/bergamot_c_api.cpp`）；
/// 3. 按清单顺序逐个声明静态库。
///
/// ## 为什么要 whole-archive
///
/// Rust 把 rlib 排在所有 native static lib **之后**。包装层在 rlib 里，
/// 它对 Bergamot 主库的符号引用要等扫到主库时才进未决表 ——
/// 若主库此时已被扫过（obj 未被拉入），就 LNK2019「未定义符号」。
/// `+whole-archive` 强制拉入主库全部 obj（主库很小，代价可忽略）。
///
/// ## 顺序
///
/// 清单顺序 = CMake 的拓扑序 = 「引用者在前、被引用者在后」，
/// 这正是静态链接需要的顺序。
///
/// ## 为什么**要**链 sentencepiece
///
/// Windows 端**不能**链 sentencepiece —— sherpa-onnx 的 static 库里
/// 带了一份完整的 sentencepiece + protobuf-lite，符号完全相同 → LNK2005。
/// 但 Android 端 sherpa 用的是 **shared** 包（`libsherpa-onnx-c-api.so`），
/// 不含 sentencepiece → **必须**链，否则翻译器初始化时缺符号。
fn link_android() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| manifest.clone());
    let bergamot_src = repo_root.join("third_party/bergamot-translator");

    // 清单：交叉编译脚本产出
    let list_path = std::env::var("BERGAMOT_LIBS_LIST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| bergamot_src.join("build-android/bergamot_libs.txt"));

    let Ok(text) = std::fs::read_to_string(&list_path) else {
        println!(
            "cargo:warning=读不到 Bergamot 链接清单 {}，链接会失败。\n\
             请先执行：bash Android/scripts/build_bergamot_android.sh",
            list_path.display()
        );
        return;
    };

    // 编译 C ABI 包装层（必须 whole-archive，且排在主库之前）
    build_c_api(&bergamot_src);

    let mut n = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p = PathBuf::from(line);
        let Some(file_name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // libfoo.a → foo
        let stem = file_name
            .trim_start_matches("lib")
            .trim_end_matches(".a");
        if stem.is_empty() {
            continue;
        }
        let Some(dir) = p.parent() else { continue };
        println!("cargo:rustc-link-search=native={}", dir.display());

        if stem == "bergamot-translator" {
            // 主库：whole-archive 强制拉入全部 obj
            println!("cargo:rustc-link-lib=static:+whole-archive=bergamot-translator");
        } else {
            println!("cargo:rustc-link-lib=static={stem}");
        }
        n += 1;
    }

    // ★ 补 C++ 标准库（Android）。
    //
    // 为什么必须手动加：`build_c_api` 里用了 `build.cargo_metadata(false)`
    // 来抑制 cc crate 输出自己的 link 指令（因为我们要输出
    // `+whole-archive` 版本）。但 cc crate 在 Android 上本来会自动输出
    // `cargo:rustc-link-lib=static=c++_static`（NDK 默认静态 STL），
    // 一并被抑制了 → 链接期报一堆
    // "undefined symbol: std::__ndk1::basic_istream<...>::read"
    // / "operator new(unsigned long)" / "__cxa_throw" 等。
    //
    // 静态库列表（Bergamot 全树）用的就是 c++_static，
    // 故这里也必须链 c++_static（不能混 c++_shared）。
    //
    // 位置：必须**在** Bergamot 静态库**之后**声明 ——
    // 静态链接对顺序敏感（引用者在前、被引用者在后），
    // 而 Bergamot 的 obj 引用 C++ 标准库符号。
    if target_os == "android" {
        if let Some(ndk) = android_ndk_root() {
            // ⚠️⚠️ **必须**把 API 级目录 `…/usr/lib/aarch64-linux-android/<api>`
            //    加进链接搜索路径！
            //
            // 为什么：NDK 的 sysroot 里有**两份** bionic：
            //   usr/lib/aarch64-linux-android/libc.a     ← 静态归档
            //   usr/lib/aarch64-linux-android/29/libc.so  ← **动态 stub**（真 ELF）
            //
            // Rust 链接 Android 时用 `-lc`，但它**不会**像 clang driver 那样
            // 自动加 API 级目录 → `-lc` 只找到 `libc.a` → **静态链入 bionic**。
            //
            // 后果（2026-10-28 真机实测，启动即闪退）：
            //   bionic 的静态构造器 `init_have_lse_atomics`
            //   （在 libc.a 的 aarch64.c 里）会在 dlopen 阶段执行，
            //   而它依赖的 bionic 全局状态是由**系统动态 libc.so** 初始化的
            //   —— 我们静态复制了代码、却没有那份状态 →
            //   `__bionic_getauxval()` 解引用空指针 → SIGSEGV。
            //
            // 崩溃栈（符号化后）：
            //   #00 __bionic_getauxval(unsigned long, bool*)
            //   #01 getauxval
            //   #02 init_have_lse_atomics
            //   #03 soinfo::call_constructors   ← dlopen 时的静态初始化
            //   #04 do_dlopen
            //
            // 加上这个目录后，`-lc` 优先命中**动态 stub** →
            // bionic 保持动态、初始化交给系统 → 不崩。
            let api = std::env::var("ANDROID_API_LEVEL").unwrap_or_else(|_| "29".to_string());
            let lib_dir = ndk.join(
                "toolchains/llvm/prebuilt/windows-x86_64/sysroot/usr/lib/aarch64-linux-android",
            );
            let api_dir = lib_dir.join(&api);

            // ⚠️⚠️ **顺序至关重要**：API 级目录（含动态 libc.so stub）
            //    必须排在 **前面**！
            //
            // 链接器按 `-L` 的先后找库：若 `…/aarch64-linux-android`（含 libc.a）
            // 在前，`-lc` 就先命中**静态** libc.a → 又变回静态 bionic → 启动闪退。
            // 把含 libc.so 的 API 级目录放前面，`-lc` 才会命中动态 stub。
            if api_dir.is_dir() {
                println!("cargo:rustc-link-search=native={}", api_dir.display());
            } else {
                println!(
                    "cargo:warning=NDK API 级目录不存在：{}",
                    api_dir.display()
                );
            }
            println!("cargo:rustc-link-search=native={}", lib_dir.display());
            if api_dir.is_dir() {

                // ★ Android 的 CRT（.so 专用）：crtbegin_so.o / crtend_so.o。
                //
                // NDK 官方链接姿势（实测 clang -v）：
                //   crtbegin_so.o …obj… libclang_rt.builtins.a -l:libunwind.a
                //   -ldl -lc … crtend_so.o
                //
                // crtbegin_so.o 负责 .so 的初始化/析构入口
                // （.init_array / .fini_array 的正确处理）。
                // Rust 的链接命令**不会**自动加它们（那是 clang driver 的职责），
                // 我们这里走手动 linker 参数 → 必须自己补。
                //
                // ⚠️ `cargo:rustc-link-arg` 只能把参数加到链接命令**末尾**，
                //    而 crtbegin_so.o 理想上应在最前。对 .so 而言它主要标记
                //    .init_array 的边界，缺失不会导致崩溃 ——
                //    真正解决崩溃的是上面的「API 级目录 → 动态 libc.so」。
                let crt_begin = api_dir.join("crtbegin_so.o");
                let crt_end = api_dir.join("crtend_so.o");
                if crt_begin.is_file() {
                    println!("cargo:rustc-link-arg={}", crt_begin.display());
                }
                if crt_end.is_file() {
                    println!("cargo:rustc-link-arg={}", crt_end.display());
                }
            } else {
                println!(
                    "cargo:warning=NDK API 级目录不存在：{}",
                    api_dir.display()
                );
            }

            // ── C++ 运行时（静态）──
            //
            // NDK 的 C++ 运行时拆成两个归档（LLVM 17+）：
            //   libc++_static.a —— C++ 标准库本体（容器、算法、iostream…）
            //   libc++abi.a    —— ABI 层（__cxa_throw / typeinfo / vtable /
            //                      unwinder 胶水）
            // 只链前者会缺一大批 undefined symbol（std::terminate、
            // __cxa_throw、typeinfo for std::bad_alloc…）→ 两个都要链。
            //
            // ⚠️ 顺序：libc++_static 在前、libc++abi 在后 ——
            //    静态链接对顺序敏感（引用者在前、被引用者在后）。
            println!("cargo:rustc-link-lib=static=c++_static");
            println!("cargo:rustc-link-lib=static=c++abi");

            // ★ compiler-rt builtins：ARM 栈保护的配套符号。
            //
            // 现象：链接期报 "undefined symbol: __arm_za_disable"。
            // 原因：NDK 的 clang 默认启用 ARM 的
            // `-mstack-protector-guard=tz`（zone-based 栈保护），
            // 被保护的函数会引用 `__arm_za_disable` 这类 helper，
            // 它们定义在 compiler-rt 里（libclang_rt.builtins-aarch64-android.a），
            // 而 Rust 的链接命令行不会自动加它。
            //
            // 路径里的 clang 版本号随 NDK 变 → 扫一遍取第一个匹配。
            if let Some(rt) = find_compiler_rt(&ndk) {
                println!(
                    "cargo:rustc-link-search=native={}",
                    rt.parent().unwrap().display()
                );
                println!("cargo:rustc-link-lib=static=clang_rt.builtins-aarch64-android");
            } else {
                println!("cargo:warning=未找到 compiler-rt builtins，ARM 栈保护符号可能缺失");
            }
        } else {
            println!("cargo:warning=找不到 ANDROID_NDK，无法链接 c++_static");
        }
    }

    // ★ Bionic 的 libc.a 自带 `rust_eh_personality` → 与 Rust std 重复。
    //
    // 现象：链接期报
    //   "duplicate symbol: rust_eh_personality
    //    >>> defined at gcc.rs:329 (library/std/src/sys/personality/gcc.rs:329)
    //    >>> defined at gcc.rs:217 (prebuilts/rust-toolchain/…/gcc.rs:217)"
    // 后者来自 NDK 的 `sysroot/usr/lib/aarch64-linux-android/libc.a`。
    //
    // 根因：LLVM 17 起 bionic 为了兼容 Rust，在 libc.a 里也提供了
    // `rust_eh_personality`（Rust 侧 personality 的 C ABI 适配器）。
    // 而 Rust std 自己也有一个 → 两者定义**同一个符号**。
    //
    // 这是 **NDK 官方记录的已知冲突**，官方推荐的处理就是
    // `-Wl,--allow-multiple-definition`：两边的实现都只是把
    // unwinding 转交给 `__cxa_*` / `_Unwind_*` 的薄适配层，行为等价，
    // 选哪个都不影响 Rust 的 panic 语义。
    //
    // 为什么以前没遇到：Android 端此前没链任何 C++ 静态库
    // （mt-bergamot 未启用）→ 根本不需要 bionic 之外的 C++ 运行时，
    // 也就没把 libc.a 拉进来与 Rust std 撞车。
    println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");

    println!("cargo:warning=Bergamot Android 链接清单已应用（{n} 个静态库）");
}

/// 在 NDK 里定位 compiler-rt builtins 库（`libclang_rt.builtins-aarch64-android.a`）。
///
/// ## 为什么需要扫目录
///
/// 路径里的 clang 版本号随 NDK 变化（r27c 是 18、r30 是 21…）：
///   <ndk>/toolchains/llvm/prebuilt/<host>/lib/clang/<ver>/lib/linux/
/// 硬编码版本号会在 NDK 升级时失效 → 扫 `lib/clang/*/lib/linux/` 取第一个匹配。
fn find_compiler_rt(ndk: &Path) -> Option<PathBuf> {
    let clang_dir = ndk.join("toolchains/llvm/prebuilt");
    let host_dir = std::fs::read_dir(&clang_dir).ok()?;
    for host in host_dir.flatten() {
        let versions = host.path().join("lib/clang");
        let Ok(vers) = std::fs::read_dir(&versions) else { continue };
        for v in vers.flatten() {
            let cand = v
                .path()
                .join("lib/linux/libclang_rt.builtins-aarch64-android.a");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 定位 NDK 根目录（用于找 `libc++_static.a`）。
///
/// 优先用 `ANDROID_NDK` 环境变量（交叉编译脚本会导出）；
/// 缺失时从 `CC_aarch64_linux_android` 之类的编译器路径反推
/// （`<ndk>/toolchains/llvm/prebuilt/<host>/bin/clang`）。
fn android_ndk_root() -> Option<PathBuf> {
    if let Ok(ndk) = std::env::var("ANDROID_NDK") {
        let p = PathBuf::from(&ndk);
        if p.is_dir() {
            return Some(p);
        }
    }
    // 反推：<ndk>/toolchains/llvm/prebuilt/<host>/bin/<cc>
    for key in [
        "CC_aarch64_linux_android",
        "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER",
    ] {
        let Ok(cc) = std::env::var(key) else { continue };
        let p = PathBuf::from(&cc);
        // .../toolchains/llvm/prebuilt/<host>/bin/<name>
        let Some(prebuilt) = p.parent().and_then(|d| d.parent()) else { continue };
        let Some(toolchains) = prebuilt.parent() else { continue };
        let Some(root) = toolchains.parent() else { continue };
        if root.join("toolchains").is_dir() {
            return Some(root.to_path_buf());
        }
    }
    None
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
    // ⚠️ 必须按 **target** 判断（CARGO_CFG_TARGET_OS），不能用 `cfg!(windows)`
    //    —— build.rs 是给 host 编译的，`cfg!` 求值的是构建机。
    let target_is_windows =
        std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let lib_dir = if target_is_windows {
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
        let name = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            // Windows 静态库名无 lib 前缀（bergamot-translator.lib）
            stem
        } else {
            // Unix 风格有 lib 前缀（libfoo.a → foo）
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
