#!/usr/bin/env python3
"""Android 产物门禁（对位 Windows/scripts/verify_product.py 的角色）。

校验 build_android.sh + assembleDebug 的产物链是否完整：
  1. jniLibs 三库就位且非 mock 壳（体积 + 符号 + NEEDED）
  2. APK 存在、含 arm64 .so、权限与前台服务声明符合预期
  3. 版本串与注入值一致（防 snapshot-v0.0.0 溜出门）

⚠️ 这是门禁不是日志：任何 FAIL 项都应视为构建不通过。
用法：
    python Android/scripts/verify_android_product.py [--apk <路径>]
退出码：0 通过 / 1 失败
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import zipfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
ROOT = SCRIPT_DIR.parent.parent  # Translator/
REPO = ROOT / "translator_core"
JNI_DIR = REPO / "ui/android/app/src/main/jniLibs/arm64-v8a"

REQUIRED_SO = {
    "libtranslator_core.so",
    "libsherpa-onnx-c-api.so",
    "libonnxruntime.so",
}
# 真实 ASR 后端（shared 模式）：胶水层 ~1 MB，mock 壳 ~0.5 MB。
# 分界取 0.8 MB —— 再叠加 SherpaOnnx* 符号校验双保险。
MIN_CORE_SO_BYTES = 800_000

failures: list[str] = []


def ok(msg: str) -> None:
    print(f"  [OK]   {msg}")


def fail(msg: str) -> None:
    print(f"  [FAIL] {msg}")
    failures.append(msg)


def find_readelf() -> str | None:
    """探测 NDK 的 llvm-readelf（Windows 上无系统 readelf）。"""
    for sdk in (os.environ.get("ANDROID_HOME"), os.environ.get("ANDROID_SDK_ROOT"),
                "D:/Android/Sdk", "C:/Program Files (x86)/Android/android-sdk"):
        if not sdk:
            continue
        ndk_root = Path(sdk) / "ndk"
        if ndk_root.is_dir():
            for v in sorted(ndk_root.iterdir(), reverse=True):
                cand = v / "toolchains/llvm/prebuilt/windows-x86_64/bin/llvm-readelf.exe"
                if cand.exists():
                    return str(cand)
    return None


def readelf(args: list[str]) -> str:
    r = subprocess.run(args, capture_output=True, text=True, timeout=60)
    return r.stdout


def check_so() -> None:
    print("\n=== jniLibs 三库 ===")
    for name in sorted(REQUIRED_SO):
        p = JNI_DIR / name
        if not p.exists():
            fail(f"缺少 {name}（先跑 Android/scripts/build_android.sh --release）")
    core = JNI_DIR / "libtranslator_core.so"
    if not core.exists():
        return
    size = core.stat().st_size
    if size < MIN_CORE_SO_BYTES:
        fail(f"libtranslator_core.so 仅 {size/1024:.0f} KB —— mock 壳（未链 ASR）")
    else:
        ok(f"libtranslator_core.so {size/1024/1024:.1f} MB")

    readelf_bin = find_readelf()
    if not readelf_bin:
        fail("找不到 llvm-readelf（NDK 未装或不在默认路径），无法校验链接完整性")
        return

    dyn = readelf([readelf_bin, "-d", str(core)])
    needed = re.findall(r"NEEDED.*\[(.+?)\]", dyn)
    if "libsherpa-onnx-c-api.so" not in needed:
        fail(f"core 未 NEEDED sherpa-c-api（实际 {needed}）—— shared 链接没生效")
    else:
        ok("NEEDED 含 libsherpa-onnx-c-api.so")
    if "libc.so" not in needed:
        fail("core 不依赖动态 libc.so —— 疑似 crt-static 残留（查 core/.cargo/config.toml）")
    else:
        ok("动态 bionic（NEEDED libc.so）")

    syms = readelf([readelf_bin, "--dyn-syms", str(core)])
    if "translator_session_push_chunk" not in syms:
        fail("缺 translator_session_push_chunk 导出（Android 采集注入链路依赖它）")
    else:
        ok("导出 translator_session_push_chunk 就位")
    # sherpa-onnx C API 真实符号前缀是 SherpaOnnx*（shared 模式下为 UND 引用）
    if "SherpaOnnx" not in syms:
        fail("无 SherpaOnnx* 符号 —— 产物里没有真实 ASR 后端")
    else:
        ok("SherpaOnnx* 引用存在（真实 ASR 后端）")

    # 版本串：门禁注入生效（防 snapshot-v0.0.0）
    #
    # 格式：{端标识}-{渠道}-v{X.Y.Z}-{YYYY}.{MMDD}.{HH}.{mm}
    # 例：android-beta-v0.9.7-2026.1028.09.35
    # 端标识全小写（windows / android），放在渠道前面。
    with open(core, "rb") as f:
        blob = f.read()
    m = re.findall(
        rb"(?:windows|android)-(?:snapshot|beta|release|alpha|pre|stable|current)"
        rb"-v\d+\.\d+\.\d+-\d{4}\.\d{4}\.\d{2}\.\d{2}",
        blob,
    )
    if not m:
        fail(
            "产物中找不到规范版本串（{端标识}-{channel}-v{X.Y.Z}-{time}）"
            "—— 构建未注入 TRANSLATOR_PLATFORM/SEMVER/CHANNEL"
        )
    else:
        first = m[0].decode()
        if first.startswith("android-snapshot-v0.0.0"):
            fail(
                f"版本串仍是默认值：{first} —— "
                "构建时须设 TRANSLATOR_SEMVER/TRANSLATOR_CHANNEL"
            )
        else:
            ok(f"版本串就位：{first}")


def check_apk(apk: Path | None) -> None:
    print("\n=== APK ===")
    if apk is None:
        default = REPO / "ui/build/app/outputs/flutter-apk/app-debug.apk"
        apk = default if default.exists() else None
    if apk is None:
        fail("未找到 APK（cd ui && flutter build apk --debug）")
        return
    ok(f"APK: {apk}（{apk.stat().st_size/1024/1024:.1f} MB）")
    with zipfile.ZipFile(apk) as z:
        names = z.namelist()
        lib_arm64 = [n for n in names if n.startswith("lib/arm64-v8a/")]
        present = {Path(n).name for n in lib_arm64}
        missing = REQUIRED_SO - present
        if missing:
            fail(f"APK 缺 arm64 库：{sorted(missing)}")
        else:
            ok(f"APK 含三库（arm64-v8a）")
        if any(n.startswith("lib/x86_64/") or n.startswith("lib/armeabi") for n in names):
            ok("附带非 arm64 ABI（提醒：真机不需要，可精简包体）")
        # manifest 无法离线解析出权限，交给 aapt2（有就用，没有只查 so）
        aapt2 = None
        for bt in (Path("D:/Android/Sdk/build-tools"),):
            if bt.is_dir():
                for v in sorted(bt.iterdir(), reverse=True):
                    cand = v / "aapt2.exe"
                    if cand.exists():
                        aapt2 = str(cand)
                        break
        if aapt2:
            r = subprocess.run([aapt2, "dump", "badging", str(apk)],
                               capture_output=True, text=True, timeout=60)
            out = r.stdout
            for perm in ("android.permission.RECORD_AUDIO",
                         "android.permission.FOREGROUND_SERVICE"):
                if perm in out:
                    ok(f"权限声明：{perm}")
                else:
                    fail(f"APK 缺权限声明 {perm}")

            # service 声明：badging **不输出** service，必须查 xmltree
            # （2026-10-26：曾因用 badging 查 service 而误报「找不到
            #  AudioCaptureService」，实际 service 早已正确打进 APK）
            r2 = subprocess.run(
                [aapt2, "dump", "xmltree", "--file", "AndroidManifest.xml", str(apk)],
                capture_output=True, text=True, timeout=60)
            manifest = r2.stdout
            if "AudioCaptureService" in manifest:
                ok("采集前台服务 AudioCaptureService 已声明")
            else:
                fail("APK manifest 里找不到 AudioCaptureService（前台服务缺失 → 采集起不来）")
            if "com.translator.app.MainActivity" in manifest:
                ok("主 Activity 已声明")
            else:
                fail("APK manifest 里找不到 MainActivity")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--apk", type=Path, default=None)
    args = ap.parse_args()
    print("=" * 60)
    print("Translator Android 产物门禁")
    print("=" * 60)
    check_so()
    check_apk(args.apk)
    print("\n" + "=" * 60)
    if failures:
        print(f"[FAIL] {len(failures)} 项不通过 —— 产物不可交付")
        return 1
    print("[PASS] 产物门禁通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
