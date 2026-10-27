#!/usr/bin/env python3
"""Android 构建环境自检。

一条命令告诉你「缺什么、装什么」，避免逐步踩坑。

用法：
    python Android/scripts/check_android_env.py
退出码：0 全部就绪 / 1 有缺失
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent / "translator_core"


def ok(msg: str) -> None:
    print(f"  [OK]   {msg}")


def miss(msg: str, fix: str) -> None:
    print(f"  [MISS] {msg}")
    print(f"         修复: {fix}")


def run(cmd: list[str], timeout: int = 30) -> tuple[bool, str]:
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
            cwd=str(REPO),
        )
        return r.returncode == 0, (r.stdout + r.stderr).strip()
    except (subprocess.SubprocessError, FileNotFoundError, OSError):
        return False, ""


def detect_sdk() -> str:
    """定位 Android SDK：env → ui/android/local.properties → 常见安装位置。

    ⚠️ 旧版兜底值是 r"D:AndroidSdk"（反斜杠被 sed 改坏，路径无效），
    2026-10-26 修复：真实读取 local.properties 的 sdk.dir，再退常见位置。
    """
    sdk = os.environ.get("ANDROID_HOME") or os.environ.get("ANDROID_SDK_ROOT")
    if sdk and Path(sdk).is_dir():
        return sdk
    lp = REPO / "ui" / "android" / "local.properties"
    if lp.exists():
        for line in lp.read_text(encoding="utf-8", errors="ignore").splitlines():
            if line.startswith("sdk.dir="):
                cand = line.split("=", 1)[1].replace("\\\\", "\\").replace("\\", "/")
                if Path(cand).is_dir():
                    return cand
    for cand in ("D:/Android/Sdk",
                 str(Path.home() / "AppData/Local/Android/Sdk"),
                 "C:/Program Files (x86)/Android/android-sdk"):
        if Path(cand).is_dir():
            return cand
    return sdk or ""


def check_android_sdk() -> list[str]:
    print("\n=== Android SDK ===")
    sdk = detect_sdk()
    if not sdk or not Path(sdk).is_dir():
        miss("SDK 目录不存在", "安装 Android Studio 并让其创建 SDK")
        return []

    ok(f"SDK: {sdk}")

    problems = []

    platforms = Path(sdk) / "platforms"
    if platforms.is_dir():
        installed = sorted(p.name for p in platforms.iterdir())
        ok(f"platforms: {', '.join(installed)}")
        if "android-34" not in installed:
            miss(
                "缺少 android-34（工程 compileSdk = 34）",
                "android --sdk=<SDK> sdk install platforms;android-34",
            )
            problems.append("platform-34")
    else:
        miss("无 platforms", "android --sdk=<SDK> sdk install platforms;android-34")
        problems.append("platforms")

    bt = Path(sdk) / "build-tools"
    if bt.is_dir():
        ok(f"build-tools: {', '.join(sorted(p.name for p in bt.iterdir()))}")
    else:
        miss("无 build-tools", "android --sdk=<SDK> sdk install build-tools;36.0.0")
        problems.append("build-tools")

    ndk = Path(sdk) / "ndk"
    if ndk.is_dir():
        versions = sorted(p.name for p in ndk.iterdir())
        ok(f"NDK: {', '.join(versions)}")
    else:
        miss(
            "NDK 未安装（Rust 交叉编译 .so 必需）",
            "Android Studio → SDK Tools → NDK (Side by side)",
        )
        problems.append("ndk")

    cmd = Path(sdk) / "cmdline-tools/latest/bin"
    ok(f"cmdline-tools: {'就绪' if cmd.is_dir() else '缺失'}")

    return problems


def find_exe(names: list[str]) -> str | None:
    """在 PATH 与常见安装位置查找**可执行**文件。

    两个坑：
    1. 子进程不一定继承调用方 PATH（Git Bash export 后调 python 就会丢），
       故除 which() 外还探测常见安装位置。
    2. Windows 上无扩展名的 Unix 脚本不可直接 exec（WinError 193），
       必须优先返回 .exe / .bat / .cmd。
    """
    # Windows 上无扩展名的是 Unix shell 脚本，Python 无法 exec（WinError 193）。
    # Git Bash 的 PATH 会把它算作"可执行"，但 subprocess 不认 —— 故 Windows 跳过。
    is_win = os.name == "nt"
    exts = (".exe", ".bat", ".cmd") if is_win else ("", ".exe", ".bat", ".cmd")
    cands = [
        Path.home() / ".cargo/bin",
        "D:/flutter/flutter/bin",
        Path.home() / "flutter/flutter/bin",
        "C:/flutter/flutter/bin",
    ]

    # 先 PATH（按扩展名优先级，避免拿到 Unix 脚本）
    for ext in exts:
        for n in names:
            w = shutil.which(n + ext)
            if w:
                return w

    # 再常见位置
    for d in cands:
        for ext in exts:
            for n in names:
                p = Path(d) / (n + ext)
                if p.exists():
                    return str(p)
    return None


def check_rust() -> list[str]:
    print("\n=== Rust 工具链 ===")
    problems = []
    rustc = find_exe(["rustc"])
    rustup = find_exe(["rustup"])
    if rustc:
        ok(f"rustc: {rustc}")
    else:
        miss("rustc 不可用", "安装 Rust")
        return problems

    installed = ""
    if rustup:
        r = subprocess.run([rustup, "target", "list", "--installed"],
                           capture_output=True, text=True, cwd=str(REPO))
        installed = r.stdout
    ok(f"targets: {installed.replace(chr(10), ', ')}")

    if "aarch64-linux-android" not in installed:
        miss(
            "缺少 aarch64-linux-android target",
            "rustup target add aarch64-linux-android",
        )
        problems.append("rust-target")

    # linker 探测（SDK 定位与 check_android_sdk 同源，勿再写死默认路径）
    sdk = detect_sdk()
    ndk_root = Path(sdk) / "ndk" if sdk else Path("_nonexist_")
    if not ndk_root.is_dir():
        # NDK 独立安装位置兜底（本机曾有 C:/Program Files (x86)/Android/AndroidNDK）
        for cand in ("C:/Program Files (x86)/Android/AndroidNDK",):
            if Path(cand).is_dir():
                sub = sorted(Path(cand).iterdir())
                if sub:
                    ndk_root = sub[0]
                    break
    if ndk_root.is_dir():
        root = ndk_root if (ndk_root / "toolchains").is_dir() else ndk_root / sorted(p.name for p in ndk_root.iterdir())[-1]
        linker = root / "toolchains/llvm/prebuilt/windows-x86_64/bin/aarch64-linux-android24-clang.cmd"
        if linker.exists():
            ok(f"linker: {linker}")
        else:
            miss("linker 不存在（需 .cmd 包装器，Windows 上 Rust 不能 exec Unix 脚本）",
                 "确认 NDK 是 windows 版（不是 linux 版）")
            problems.append("linker")

    return problems


def check_flutter() -> list[str]:
    print("\n=== Flutter ===")
    flutter = find_exe(["flutter"])
    if not flutter:
        miss("Flutter 未安装（UI 无法编译）", "bash scripts/setup_flutter.sh")
        return ["flutter"]

    ok(f"flutter: {flutter}")
    r = subprocess.run([flutter, "--version"], capture_output=True, text=True, cwd=str(REPO))
    if r.returncode == 0:
        ok(r.stdout.splitlines()[0][:60])
    else:
        miss("flutter 存在但无法执行", "检查 PATH 或重装")
        return ["flutter"]
    return []


def check_project() -> list[str]:
    print("\n=== 工程配置 ===")
    problems = []
    ui = REPO / "ui"
    if not (ui / "pubspec.yaml").exists():
        miss("ui/pubspec.yaml 不存在", "检查工程结构")
        problems.append("pubspec")
    else:
        ok("pubspec.yaml 存在")

    # flutter config 的 android-sdk 是否指向有效目录
    # ⚠️ 旧版正则 `android-sdk://s*(//S.*)` 把反斜杠全丢了（永不匹配），
    # 2026-10-26 修复：flutter config 输出形如 "    android-sdk: D:\Android\Sdk"
    fl = find_exe(["flutter"])
    if fl:
        r = subprocess.run([fl, "config"], capture_output=True, text=True, cwd=str(REPO))
        import re as _re
        for line in r.stdout.splitlines():
            m = _re.match(r"^\s*android-sdk:\s*(\S.*)$", line)
            if m:
                sdk = m.group(1).strip().rstrip(",")
                if Path(sdk).is_dir():
                    ok(f"flutter config android-sdk: {sdk}")
                else:
                    miss(f"flutter config android-sdk 指向无效目录: {sdk}",
                         "flutter config --android-sdk <有效路径>")

    android = ui / "android"
    # ⚠️ 工程实际是 Kotlin DSL：settings.gradle.kts（旧版查 settings.gradle
    # 永远误报缺失）。两个名字都认，取其一即可。
    if not ((android / "settings.gradle.kts").exists() or (android / "settings.gradle").exists()):
        miss("android/settings.gradle(.kts) 不存在", "检查工程结构")
        problems.append("settings.gradle")
    else:
        ok("settings.gradle(.kts) 存在")

    if not (android / "gradle/wrapper/gradle-wrapper.properties").exists():
        miss("gradle-wrapper.properties 不存在", "检查工程结构")
        problems.append("wrapper")
    else:
        ok("gradle wrapper 配置存在")

    so = android / "app/src/main/jniLibs/arm64-v8a/libtranslator_core.so"
    if not so.exists():
        miss(
            "libtranslator_core.so 未生成（Rust 核心库）",
            "bash Android/scripts/build_android.sh --release",
        )
        problems.append("so")
    else:
        size = so.stat().st_size
        ok(f"libtranslator_core.so 已生成（{size/1024/1024:.1f} MB）")
        # mock 壳与真实引擎的分界：shared 模式胶水层 ~1.2 MB，
        # 纯 mock 约 0.48 MB（2026-10-26 实测两档）
        if size < 800_000:
            miss(
                f"产物仅 {size/1024:.0f} KB —— 疑似 mock 壳（未链 sherpa）",
                "先跑 Android/scripts/fetch_sherpa_android.sh，再重新 build_android.sh",
            )
            problems.append("so-mock")

    # sherpa 运行库（Android shared 模式必须随 APK 分发）
    jnilibs = android / "app/src/main/jniLibs/arm64-v8a"
    for lib in ("libsherpa-onnx-c-api.so", "libonnxruntime.so"):
        if (jnilibs / lib).exists():
            ok(f"{lib} 已就位")
        else:
            miss(f"{lib} 缺失（APK 运行期 dlopen 失败 → 引擎起不来）",
                 "bash Android/scripts/build_android.sh --release（会自动拷贝）")
            problems.append(f"lib:{lib}")

    # sherpa 预编译源包（构建输入）
    if (REPO / "third_party/sherpa-prebuilt/android/jniLibs/arm64-v8a").is_dir():
        ok("sherpa-onnx Android 包已落盘 third_party")
    else:
        miss("sherpa-onnx Android 包缺失（交叉编译链接输入）",
             "bash Android/scripts/fetch_sherpa_android.sh")
        problems.append("sherpa-prebuilt")

    if not (ui / "pubspec.lock").exists():
        print("  [INFO] pubspec.lock 未生成 → 需运行 flutter pub get")
    else:
        ok("依赖已锁定")

    return problems


def main() -> int:
    print("=" * 60)
    print("Translator Android 构建环境自检")
    print("=" * 60)

    all_problems: list[str] = []
    all_problems += check_android_sdk()
    all_problems += check_rust()
    all_problems += check_flutter()
    all_problems += check_project()

    print("\n" + "=" * 60)
    if not all_problems:
        print("[PASS] 环境就绪，可以构建：")
        print("       cd ui && flutter build apk --debug")
        return 0

    print(f"[FAIL] 缺失 {len(all_problems)} 项：{', '.join(all_problems)}")
    print("\n建议按顺序处理（后面的依赖前面的）：")
    print("  1. bash scripts/setup_flutter.sh     # Flutter + 国内镜像")
    print("  2. rustup target add aarch64-linux-android")
    print("  3. bash Android/scripts/build_android.sh     # 编译 .so")
    print("  4. cd ui && flutter pub get")
    print("  5. cd ui && flutter build apk --debug")
    return 1


if __name__ == "__main__":
    sys.exit(main())
