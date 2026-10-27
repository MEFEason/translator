#!/usr/bin/env python3
"""编译 Bergamot 静态库（Windows / MSVC）。

## 为什么需要这个脚本

编译 Bergamot 需要完整的 MSVC 环境（`cl.exe` / `link.exe` / `INCLUDE` /
`LIB`），而这些**只在 vcvars64.bat 执行后**才注入到进程环境里。
Bash 工具每次调用都是新 shell，`export` 留不住，所以这里：

1. 用 `vswhere` 定位 Visual Studio
2. 执行 `cmd /c "call vcvars64.bat && set"` 拿到环境变量
3. 把它们合进 `os.environ`
4. 在这个环境里调 cmake / ninja

这样任何时候运行都自洽，不依赖 shell 状态。

## 产物

`third_party/bergamot-translator/build/libbergamot-translator.a`

之后 `core/build.rs` 通过 `BERGAMOT_PREBUILT` 指向它即可跳过 CMake。

用法：
    python scripts/build_bergamot.py            # configure + build
    python scripts/build_bergamot.py --configure-only
    python scripts/build_bergamot.py --clean
    python scripts/build_bergamot.py -j 8       # 并行度
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent / "translator_core"
SRC = REPO / "third_party" / "bergamot-translator"
BUILD = SRC / "build"
LIB_NAME = "src/translator/bergamot-translator.lib"

VSWHERE = Path(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe")


def log(msg: str) -> None:
    print(msg, flush=True)


# ── 源码 patch（幂等，每次 configure 前跑）──────────────────────────────
# 为什么必须改源码而不是传 -D：
#   marian-dev/CMakeLists.txt 第 228 行「无条件」覆盖 CMAKE_CXX_FLAGS，
#   所以命令行传进来的 -DCMAKE_CXX_FLAGS=... 会被直接丢弃。
#   而 /WX 在第 212 行的 ALL_WARNINGS 里，同样没法从命令行摘掉。
MARIAN_CMAKE = SRC / "3rd_party" / "marian-dev" / "CMakeLists.txt"
MARIAN_3RDPARTY_CMAKE = SRC / "3rd_party" / "marian-dev" / "src" / "3rd_party" / "CMakeLists.txt"
SSPLIT_FIND_PCRE2 = SRC / "3rd_party" / "ssplit-cpp" / "cmake" / "FindPCRE2.cmake"
BERGAMOT_TOP_CMAKE = SRC / "CMakeLists.txt"
SENTENCEPIECE_CMAKE = (
    SRC / "3rd_party" / "marian-dev" / "src" / "3rd_party" / "sentencepiece" / "CMakeLists.txt"
)
SENTENCEPIECE_SRC_CMAKE = (
    SRC / "3rd_party" / "marian-dev" / "src" / "3rd_party" / "sentencepiece" / "src" / "CMakeLists.txt"
)

# ruy 头文件带 #pragma GCC diagnostic，MSVC 不认识 → C4068
# 「未知的杂注」。这只是「我不认识」，对 MSVC 无害，但 /W4 + /WX 会
# 让它升级成编译错误。ruy 是第三方源码，改它不如让编译器闭嘴。
#
# 每条补丁是 (文件, needle 原样, new 替换后)。幂等靠「new 已在文中则跳过」。
PATCHES: list[tuple[Path, str, str]] = [
    # 1) 去掉警告即错误
    (
        MARIAN_CMAKE,
        '  list(APPEND ALL_WARNINGS /WX; /W4;)',
        '  list(APPEND ALL_WARNINGS /W4;)\n'
        '  # [patched] 已移除 /WX：ruy 等第三方头文件会触发 C4068/C4244 等'
        '无害警告，/WX 会把它们升级为错误',
    ),
    # 2) 把 C4068 等并入全局静音列表
    (
        MARIAN_CMAKE,
        '  set(DISABLE_GLOBALLY "/wd\\"4310\\" /wd\\"4324\\" /wd\\"4702\\" /wd\\"4100\\"")',
        '  set(DISABLE_GLOBALLY "/wd\\"4310\\" /wd\\"4324\\" /wd\\"4702\\"'
        ' /wd\\"4100\\" /wd\\"4068\\" /wd\\"4244\\" /wd\\"4267\\" /wd\\"4996\\"")',
    ),
    # 2.5) CRT：保持 /MT（与 sherpa-onnx 官方 static-MT 预编译库统一）
    #
    # 历史：曾统一到 /MD（sentencepiece/Rust 默认动态 CRT），但后来接入
    # sherpa-onnx 时发现它的官方预编译库是 static-MT，改不动 →
    # 全项目只能统一 /MT。Rust 侧用 -C target-feature=+crt-static。
    # 原生 /MT 无需补丁，这里只验证状态。
    (
        MARIAN_CMAKE,
        '  set(CMAKE_CXX_FLAGS_RELEASE          "${CMAKE_CXX_FLAGS} /MT /O2 '
        '${INTRINSICS} /MP /GL /DNDEBUG")',
        '  set(CMAKE_CXX_FLAGS_RELEASE          "${CMAKE_CXX_FLAGS} /MT /O2 '
        '${INTRINSICS} /MP /GL /DNDEBUG")',
    ),
    # 3) ruy 分支：修正条件 + cpuinfo 存在性判断
    #
    # 坑 A（条件写错）：marian-dev/CMakeLists.txt:68 把 `option(USE_RUY ...)`
    #   放在 `if(... MATCHES "arm")` 里 —— **只有 ARM 架构才定义 USE_RUY**。
    #   我们在 x86_64 上，`USE_RUY` 根本不存在 → 原 `if(USE_RUY)` 恒假 →
    #   ruy 整个分支被跳过 → ruy.lib 从未生成 → 链接期才炸 LNK1181。
    #   x86 下正确的开关是同层的 USE_RUY_SGEMM（x86 默认 ON）。
    #
    # 坑 B（缺 submodule）：codeload 的 tar 不含 git submodule，
    #   cpuinfo 目录存在但为空 → 无条件 add_subdirectory 会中断 configure。
    #   ruy 上游自己就有 EXISTS 判断 + 静默降级，这里对齐上游。
    (
        MARIAN_3RDPARTY_CMAKE,
        'if(USE_RUY)\n'
        '  set(CPUINFO_BUILD_MOCK_TESTS OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_BENCHMARKS OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_PKG_CONFIG OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_UNIT_TESTS OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_TOOLS      OFF CACHE BOOL " " FORCE)\n'
        '  add_subdirectory(ruy/third_party/cpuinfo EXCLUDE_FROM_ALL)\n'
        '  add_subdirectory(ruy EXCLUDE_FROM_ALL)\n'
        'endif(USE_RUY)',
        'if(USE_RUY OR USE_RUY_SGEMM)\n'
        '  set(CPUINFO_BUILD_MOCK_TESTS OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_BENCHMARKS OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_PKG_CONFIG OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_UNIT_TESTS OFF CACHE BOOL " " FORCE)\n'
        '  set(CPUINFO_BUILD_TOOLS      OFF CACHE BOOL " " FORCE)\n'
        '  # [patched] cpuinfo 是 ruy 的 git submodule，codeload 的 tar 里\n'
        '  #   只有空目录 → 加存在性判断，对齐 ruy 上游的静默降级。\n'
        '  if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/ruy/third_party/cpuinfo/CMakeLists.txt")\n'
        '    add_subdirectory(ruy/third_party/cpuinfo EXCLUDE_FROM_ALL)\n'
        '  else()\n'
        '    message(STATUS "cpuinfo submodule 未检出，跳过（ruy 降级为无 CPU 探测）")\n'
        '  endif()\n'
        '  add_subdirectory(ruy EXCLUDE_FROM_ALL)\n'
        'endif(USE_RUY OR USE_RUY_SGEMM)',
    ),
    # 4) pcre2 库名：MSVC 下实际产物是 pcre2-8-static.lib
    #
    # FindPCRE2.cmake 原本写死 PCRE2_LIBRARIES = .../lib/pcre2-8.lib，
    # 但 pcre2 的 CMakeLists 在 MSVC 分支显式设
    # `OUTPUT_NAME pcre2-8-static` → 装出来的是 pcre2-8-static.lib。
    # 名字对不上 → 链接期 LNK1181「无法打开输入文件 lib\pcre2-8.lib」。
    # 这里让 PCRE2_LIBRARIES 跟随实际产物名。
    (
        SSPLIT_FIND_PCRE2,
        '  set(PCRE2_LIBRARIES ${CMAKE_BINARY_DIR}/${CMAKE_INSTALL_LIBDIR}/'
        '${CMAKE_STATIC_LIBRARY_PREFIX}pcre2-8${CMAKE_STATIC_LIBRARY_SUFFIX})',
        '  # [patched] MSVC 下 pcre2 的 OUTPUT_NAME 是 pcre2-8-static，\n'
        '  #   装出来的是 lib/pcre2-8-static.lib；这里原本写死 pcre2-8.lib\n'
        '  #   → 链接期 LNK1181。按平台取实际产物名。\n'
        '  IF(MSVC)\n'
        '    set(PCRE2_LIB_NAME pcre2-8-static)\n'
        '  ELSE()\n'
        '    set(PCRE2_LIB_NAME pcre2-8)\n'
        '  ENDIF()\n'
        '  set(PCRE2_LIBRARIES ${CMAKE_BINARY_DIR}/${CMAKE_INSTALL_LIBDIR}/'
        '${CMAKE_STATIC_LIBRARY_PREFIX}${PCRE2_LIB_NAME}${CMAKE_STATIC_LIBRARY_SUFFIX})',
    ),
    # 5) 顶层 CMakeLists 的 /MT → /MD
    #
    # 坑：/MT 不止出现在 marian-dev/CMakeLists.txt（第 230 行），
    # bergamot-translator **顶层** CMakeLists.txt 第 58 行也有一份。
    # app/bergamot.cpp 继承顶层 → 两处必须一致，否则链接期 LNK2038。
    # 统一 /MT（sherpa-onnx 官方预编译库就是 static-MT）。
    (
        BERGAMOT_TOP_CMAKE,
        '  set(CMAKE_CXX_FLAGS_RELEASE   "${CMAKE_CXX_FLAGS} /MT /O2 ${INTRINSICS} /MP /GL /DNDEBUG")',
        '  set(CMAKE_CXX_FLAGS_RELEASE   "${CMAKE_CXX_FLAGS} /MT /O2 ${INTRINSICS} /MP /GL /DNDEBUG")',
    ),
    # 6) sentencepiece 的 CRT：保持原生 /MT
    #
    # 坑：sentencepiece 的 CMakeLists 在 MSVC 下有**两组** REPLACE：
    #   原生组：/MD → /MT（官方强制静态 CRT）
    #   遗留组：/MT → /MD（本项目早期「统一 /MD」时期加的）
    # 两组串联的净效果是**全变 /MD**（先 MD→MT 再 MT→MD）——
    # 这正是 sentencepiece.lib 编出来是 MD、与全树 /MT 冲突的原因
    # （LNK2038 MD_DynamicRelease vs MT_StaticRelease）。
    # 当前策略是全树统一 /MT（sherpa-onnx 官方预编译库是 static-MT，
    # 改不动），所以必须**删掉遗留组**，只留原生组。
    (
        SENTENCEPIECE_CMAKE,
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_DEBUG          ${CMAKE_CXX_FLAGS_DEBUG})\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_MINSIZEREL     ${CMAKE_CXX_FLAGS_MINSIZEREL})\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_RELEASE        ${CMAKE_CXX_FLAGS_RELEASE})\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_RELWITHDEBINFO ${CMAKE_CXX_FLAGS_RELWITHDEBINFO})\n'
        '  string(REPLACE "/MT" "/MD" CMAKE_CXX_FLAGS_DEBUG          ${CMAKE_CXX_FLAGS_DEBUG})\n'
        '  string(REPLACE "/MT" "/MD" CMAKE_CXX_FLAGS_MINSIZEREL     ${CMAKE_CXX_FLAGS_MINSIZEREL})\n'
        '  string(REPLACE "/MT" "/MD" CMAKE_CXX_FLAGS_RELEASE        ${CMAKE_CXX_FLAGS_RELEASE})\n'
        '  string(REPLACE "/MT" "/MD" CMAKE_CXX_FLAGS_RELWITHDEBINFO ${CMAKE_CXX_FLAGS_RELWITHDEBINFO})',
        '  # [patched] 删除了下面这组历史遗留的 "/MT" → "/MD" 替换：\n'
        '  #   它会把原生 /MD→/MT 的结果再翻回 /MD，导致 sentencepiece\n'
        '  #   与全树 /MT 冲突（LNK2038）。全树统一 /MT。\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_DEBUG          ${CMAKE_CXX_FLAGS_DEBUG})\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_MINSIZEREL     ${CMAKE_CXX_FLAGS_MINSIZEREL})\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_RELEASE        ${CMAKE_CXX_FLAGS_RELEASE})\n'
        '  string(REPLACE "/MD" "/MT" CMAKE_CXX_FLAGS_RELWITHDEBINFO ${CMAKE_CXX_FLAGS_RELWITHDEBINFO})',
    ),
    # 8) 【历史方案，已废弃】sentencepiece 复用 sherpa 的 protobuf
    #
    # 曾试图清空 PROTOBUF_LITE_SRCS 让 sentencepiece 引用 sherpa rlib 里的
    # protobuf（消 LNK2005）。实测失败：sherpa 的 protobuf 是精简子集，
    # 缺 Arena/ExtensionSet 等符号 → LNK2001（15 个未解析）。
    #
    # 现行方案：**sherpa-onnx 改用 shared（DLL）模式**
    # （core/Cargo.toml 的 asr-sherpa feature 启用 sherpa-onnx/shared），
    # protobuf 藏进 DLL，静态符号完全不碰 → 无需任何 patch。
    # sentencepiece 保持原生行为（自己编 protobuf-lite）。

    # 9) /NODEFAULTLIB:MSVCRT 保持原样（两处）
    #
    # 这两个标记是 /MT 静态 CRT 的标配：静态链接时必须排除动态的
    # msvcrt.lib，否则符号重复。曾统一 /MD 时去掉过，现在恢复。
    # 顶层 CMakeLists 与 marian-dev/CMakeLists.txt 各有一处。
    (
        BERGAMOT_TOP_CMAKE,
        '  set(CMAKE_EXE_LINKER_FLAGS_RELEASE "${CMAKE_EXE_LINKER_FLAGS} /NODEFAULTLIB:MSVCRT")',
        '  set(CMAKE_EXE_LINKER_FLAGS_RELEASE "${CMAKE_EXE_LINKER_FLAGS} /NODEFAULTLIB:MSVCRT")',
    ),
    (
        MARIAN_CMAKE,
        '  set(CMAKE_EXE_LINKER_FLAGS_RELEASE "${CMAKE_EXE_LINKER_FLAGS} /NODEFAULTLIB:MSVCRT")',
        '  set(CMAKE_EXE_LINKER_FLAGS_RELEASE "${CMAKE_EXE_LINKER_FLAGS} /NODEFAULTLIB:MSVCRT")',
    ),
]


def patch_protobuf_namespace() -> None:
    """把 sentencepiece 的 protobuf-lite 隔离到独立命名空间（spm_google）。

    为什么必须做：
    sherpa-onnx 的 static 库自带一份 protobuf-lite（rlib 内），与这里的
    那份**符号完全相同**（`google::protobuf::internal::*`）→ LNK2005 一整屏。
    用 `/FORCE:MULTIPLE` 绕开重复定义检查虽然能链接通过，但运行时会崩：
    链接器可能选中 sherpa 那份**精简** protobuf（缺 Arena/ExtensionSet），
    而 sentencepiece 读 .spm 词表需要完整版 → ORT 初始化时 C++ 异常
    穿过 Rust 帧 →「Rust cannot catch foreign exceptions」。

    真正的解法是让两份 protobuf 符号名不同。sherpa 已经这么做了
    （它的 sentencepiece 符号是 `sentencepiece::Ssentencepiece`），
    这里照做：protobuf 的 `google` 命名空间 → `spm_google`。

    实际改写交给 scripts/patch_protobuf_ns.py（幂等）。
    """
    script = Path(__file__).with_name("patch_protobuf_ns.py")
    if not script.exists():
        log(f"[PATCH] 缺少 {script.name}，跳过 protobuf 命名空间隔离")
        return

    sp_root = SRC / "3rd_party" / "marian-dev" / "src" / "3rd_party" / "sentencepiece"
    targets = [
        sp_root / "third_party" / "protobuf-lite",
        sp_root / "src",
    ]
    cmd = [sys.executable, str(script)] + [str(t) for t in targets if t.is_dir()]
    r = subprocess.run(cmd, capture_output=True, text=True)
    for line in (r.stdout or "").splitlines():
        if line.strip():
            log(f"[PATCH] {line.strip()}")
    if r.returncode != 0:
        log(f"[PATCH] protobuf 命名空间隔离失败：{(r.stderr or '').strip()[:200]}")


def patch_sources() -> None:
    """把离线/Windows 专用补丁打进第三方源码。幂等，可重复运行。"""
    # 按文件分组，避免同一文件被反复读写
    by_file: dict[Path, list[tuple[str, str]]] = {}
    for path, needle, new in PATCHES:
        by_file.setdefault(path, []).append((needle, new))

    total = 0
    for path, items in by_file.items():
        if not path.exists():
            log(f"[PATCH] 跳过，源码不在：{path.relative_to(SRC)}")
            continue
        text = orig = path.read_text(encoding="utf-8")
        applied = 0
        for needle, new in items:
            if new in text:
                continue  # 已打过
            if needle in text:
                text = text.replace(needle, new, 1)
                applied += 1
        if text != orig:
            path.write_text(text, encoding="utf-8")
        total += applied
        log(f"[PATCH] {path.relative_to(SRC)} 应用 {applied} 处"
            + ("（已是最新）" if applied == 0 else ""))
    log(f"[PATCH] 合计 {total} 处补丁")


def find_vs() -> Path | None:
    """用 vswhere 定位 VS 安装目录。"""
    if not VSWHERE.exists():
        return None
    try:
        r = subprocess.run(
            [
                str(VSWHERE),
                "-latest",
                "-products", "*",
                "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property", "installationPath",
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if r.returncode == 0 and r.stdout.strip():
            return Path(r.stdout.strip().splitlines()[0])
    except Exception as e:  # noqa: BLE001
        log(f"[WARN] vswhere 失败: {e}")
    return None


def vcvars_env(vs: Path) -> dict[str, str]:
    """执行 vcvars64.bat 并收集它设置的环境变量。"""
    bat = vs / "VC" / "Auxiliary" / "Build" / "vcvars64.bat"
    if not bat.exists():
        raise SystemExit(f"[FAIL] 找不到 {bat}")

    # 用临时 .bat，而不是把 `call "path" && set` 直接塞进 cmd /c 的参数。
    # 原因：subprocess 在 list 形式下会自己给参数加引号，与手写的引号叠加，
    # cmd 会把整段当成命令名，报
    #   '\"C:\Program Files\...\vcvars64.bat\"' is not recognized
    tmp_bat = BUILD.parent / "_vcvars_dump.bat"
    tmp_bat.write_text(
        "@echo off\r\n"
        f'call "{bat}" >nul 2>&1\r\n'
        "set\r\n",
        encoding="ascii",
        errors="replace",
    )
    try:
        r = subprocess.run(
            ["cmd", "/c", str(tmp_bat)],
            capture_output=True,
            text=True,
            timeout=300,
        )
    finally:
        tmp_bat.unlink(missing_ok=True)
    if r.returncode != 0 or "VCToolsInstallDir" not in r.stdout:
        raise SystemExit(
            "[FAIL] vcvars64 执行失败：\n" + (r.stderr or r.stdout)[:600]
        )

    env: dict[str, str] = {}
    for line in r.stdout.splitlines():
        if "=" not in line:
            continue
        k, _, v = line.partition("=")
        k = k.strip()
        # 只取真正有用的，避免污染
        if k in ("PATH", "INCLUDE", "LIB", "LIBPATH", "LIB_ENVIRONMENT",
                 "VCToolsInstallDir", "WindowsSdkDir", "WindowsSDKVersion",
                 "UniversalCRTSdkDir", "UCRTVersion", "VSINSTALLDIR"):
            env[k] = v
    return env


def dedupe_env_keys(env: dict[str, str]) -> list[str]:
    """去掉大小写重复的环境变量。

    ## 为什么需要

    MSBuild 启动子进程时用 `Hashtable` 存环境变量，而 Hashtable
    **大小写不敏感但不区分** —— 同时存在 `HTTPS_PROXY` 与
    `https_proxy` 会直接抛：

        error MSB6001: "cmd.exe"的命令行开关无效。
        已添加项。字典中的关键字:"HTTPS_PROXY" 所添加的关键字:"https_proxy"

    某些子构建（pcre2 的 INSTALL 步骤会再起一次 MSBuild）会踩到，
    主目标却不触发，容易误判成编译错误。

    策略：保留**第一个**出现的（同大小写优先，大写优先），
    其余大小写变体直接删掉。
    """
    seen: dict[str, str] = {}
    dropped: list[str] = []
    # 大写键优先（Windows 惯例），保持稳定顺序
    for k in sorted(env.keys(), key=lambda s: (s.upper() != s.upper(), s)):
        up = k.upper()
        if up in seen:
            dropped.append(f"{k}（与 {seen[up]} 重复）")
            env.pop(k, None)
        else:
            seen[up] = k
    return dropped


def supplement_windows_sdk(env: dict[str, str]) -> list[str]:
    """手动补 Windows SDK 路径。

    ## 为什么需要这一步

    `vcvars64.bat` 靠 **reg.exe 查注册表**来确定装了哪些 Windows SDK，
    再把对应的 `bin/<ver>/<arch>` 加进 PATH。而本机的安全策略
    **禁止执行 reg.exe**，于是 vcvars 只设置到 MSVC 那一层：

        PATH 里有 …/VC/Tools/MSVC/14.51.36231/bin/HostX64/x64
        → cl.exe ✓  link.exe ✓
        → rc.exe ✗  mt.exe ✗   （这俩在 Windows SDK 里，不在 MSVC 里）

    cmake 编译 C++ 时要生成资源清单，缺 rc.exe 就报
        `RC Pass 1: command "rc /fo …" failed … no such file or directory`
        `CMAKE_MT-NOTFOUND`

    所以这里绕过注册表，直接扫描 Windows Kits 目录补齐 PATH 与 LIB。
    """
    added: list[str] = []
    kits = Path(r"C:\Program Files (x86)\Windows Kits\10")
    if not kits.exists():
        return added

    # ⚠️ `bin` 下**混着版本目录与架构目录**：
    #     bin/10.0.26100.0/  bin/x86/  bin/x64/  bin/arm64/
    # 直接字典序取最大会得到 `x86`（x > 1），必须按版本号正则过滤。
    import re as _re

    vers = sorted(
        [
            d.name
            for d in (kits / "bin").iterdir()
            if d.is_dir() and _re.fullmatch(r"\d+\.\d+\.\d+\.\d+", d.name)
        ],
        key=lambda s: tuple(int(x) for x in s.split(".")),
        reverse=True,
    )
    if not vers:
        return added
    ver = vers[0]
    arch_dir = kits / "bin" / ver / "x64"
    lib_dir = kits / "Lib" / ver / "um" / "x64"
    ucrt_dir = kits / "Lib" / ver / "ucrt" / "x64"

    parts = env.get("PATH", "").split(os.pathsep)
    for d in (arch_dir,):
        s = str(d)
        if s not in parts:
            parts.insert(0, s)
            added.append(s)

    libs = [str(p) for p in (lib_dir, ucrt_dir) if p.exists()]
    env["LIB"] = os.pathsep.join([*libs, env.get("LIB", "")]).rstrip(os.pathsep)
    added.extend(libs)

    # ★ INCLUDE 同样要补。vcvars 拿不到 SDK 时 INCLUDE 只有 3 段：
    #     MSVC/include、ATLMFC/include、VS/include
    #   缺 ucrt（assert.h / stdio.h 等 CRT 头）→ 编译报
    #     `fatal error C1083: 无法打开包括文件: "assert.h"`
    inc_root = kits / "Include" / ver
    inc_dirs = [
        inc_root / "ucrt",
        inc_root / "um",
        inc_root / "shared",
        inc_root / "winrt",
    ]
    incs = [str(p) for p in inc_dirs if p.exists()]
    if incs:
        cur = [x for x in env.get("INCLUDE", "").split(os.pathsep) if x]
        # SDK 头放最前（ucrt 必须优先于其它）
        for d in reversed(incs):
            if d not in cur:
                cur.insert(0, d)
        env["INCLUDE"] = os.pathsep.join(cur)
        added.extend(incs)

    env["PATH"] = os.pathsep.join(parts)
    return added


def tool_path(env: dict[str, str], exe: str) -> str | None:
    """在 vcvars 环境里找可执行文件。"""
    path = env.get("PATH", "")
    for d in path.split(os.pathsep):
        p = Path(d) / exe
        if p.exists():
            return str(p)
    return None


def run(cmd: list[str], env: dict[str, str], cwd: Path, label: str) -> bool:
    log(f"  → {label}")
    t0 = time.time()
    r = subprocess.run(cmd, env=env, cwd=str(cwd), capture_output=True, text=True)
    dt = time.time() - t0
    if r.returncode != 0:
        out = r.stdout or ""
        err = r.stderr or ""
        # 编译输出动辄上千行，直接截尾会把真正的错误冲掉。
        # 先挑出含 error / FAILED 的行，再补上下文。
        lines = out.splitlines() + err.splitlines()
        hits = [
            i for i, ln in enumerate(lines)
            if "error C" in ln or "error LNK" in ln
            or ln.startswith("FAILED")
            or "CMake Error" in ln
        ]
        log(f"  ✗ 失败（{dt:.0f}s），关键错误 {len(hits)} 条：")
        shown = set()
        for i in hits[:12]:
            for j in range(max(0, i - 2), min(len(lines), i + 5)):
                if j not in shown:
                    shown.add(j)
                    log(f"    {lines[j]}")
            log("    ---")
        if not hits:
            for ln in lines[-30:]:
                log(f"    {ln}")
        return False
    log(f"  ✓ 完成（{dt:.0f}s）")
    return True


def main() -> int:
    ap = argparse.ArgumentParser(description="编译 Bergamot 静态库")
    ap.add_argument("--configure-only", action="store_true", help="只 configure")
    ap.add_argument("--clean", action="store_true", help="先删 build 目录")
    ap.add_argument("-j", "--jobs", type=int, default=0, help="并行度（0=自动）")
    ap.add_argument("-t", "--target", default=LIB_NAME,
                    help=f"cmake 目标（默认 {LIB_NAME}；编 CLI 用 bergamot）")
    args = ap.parse_args()

    log("=" * 62)
    log("Bergamot 静态库编译（MSVC）")
    log("=" * 62)

    if not SRC.exists():
        log(f"[FAIL] 缺源码：{SRC}")
        log("       先跑：python scripts/fetch_bergamot.py")
        return 1

    if args.clean and BUILD.exists():
        shutil.rmtree(BUILD, ignore_errors=True)
        log("[CLEAN] 已删除 build/")

    # ── 载入 MSVC 环境 ──
    vs = find_vs()
    if vs is None:
        log("[FAIL] 找不到 Visual Studio（vswhere 无输出）")
        return 1
    log(f"[VS] {vs}")
    env = dict(os.environ)
    try:
        venc = vcvars_env(vs)
    except SystemExit as e:
        log(str(e))
        return 1
    env.update(venc)
    for k in ("PATH", "INCLUDE", "LIB"):
        env[k] = venc.get(k, env.get(k, ""))

    dropped = dedupe_env_keys(env)
    if dropped:
        log(f"[ENV] 去掉大小写重复的变量 {len(dropped)} 个: "
            + ", ".join(dropped[:4]))

    added = supplement_windows_sdk(env)
    if added:
        log(f"[SDK] 手动补 {len(added)} 条路径（reg.exe 被禁，vcvars 取不到 SDK）")
    log(f"[ENV] cl      = {tool_path(env, 'cl.exe') or '（未找到）'}")
    log(f"[ENV] rc      = {tool_path(env, 'rc.exe') or '（未找到）'}")
    log(f"[ENV] mt      = {tool_path(env, 'mt.exe') or '（未找到）'}")

    cl = tool_path(venc, "cl.exe")
    cmake = tool_path(venc, "cmake.exe") or shutil.which("cmake") or ""
    ninja = tool_path(venc, "ninja.exe")
    if not cmake:
        # 兜底：VS 自带 cmake 的固定位置
        for cand in (
            vs / "Common7" / "IDE" / "CommonExtensions" / "Microsoft"
            / "CMake" / "CMake" / "bin" / "cmake.exe",
        ):
            if cand.exists():
                cmake = str(cand)
                break
    log(f"[ENV] cmake   = {cmake or '（未找到）'}")
    log(f"[ENV] ninja   = {ninja or '（用默认生成器）'}")
    rc = tool_path(env, "rc.exe")
    if rc:
        # 必须用正斜杠：CMake 把 CMAKE_RC_COMPILER 写进 CMakeRCCompiler.cmake，
        # 反斜杠会被当成 cmake 字符串的转义符（Invalid character escape '\P'）。
        # 正斜杠 CMake 一样认，且路径含空格由 subprocess 自动加引号。
        cfg_rc = f"-DCMAKE_RC_COMPILER={rc.replace(chr(92), '/')}"
    else:
        cfg_rc = ""
    if not cl or not cmake:
        log("[FAIL] 工具链不完整")
        return 1

    # ── configure ──
    log("")
    patch_sources()
    patch_protobuf_namespace()
    log("[1/2] configure")
    gen = ["-G", "Ninja"] if ninja else ["-G", "NMake Makefiles"]
    cfg = [
        cmake, "-S", str(SRC), "-B", str(BUILD),
        *gen,
        "-DCMAKE_BUILD_TYPE=Release",
        # 关掉 WASM 兼容源（官方文档对 Windows 交叉编译的建议）
        "-DUSE_WASM_COMPATIBLE_SOURCES=off",
        "-DCOMPILE_TESTS=OFF",
        "-DCOMPILE_PYTHON=OFF",
        "-DCOMPILE_WASM=OFF",
        # 不做 submodule 检查（我们已手动下好）
        "-DGIT_SUBMODULE=OFF",
        # sentencepiece 声明 cmake_minimum_required(VERSION <3.5)，
        # 而 CMake 4.x 已移除对 <3.5 的兼容 → 用这个开关继续。
        "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
        # ssplit 需要 pcre2 做句子切分。系统没装 pcre2，
        # 走它自带的 ExternalProject 模式，用我们放在
        # ssplit-cpp/src/3rd-party/pcre2-10.39/ 的本地源码编译。
        "-DSSPLIT_USE_INTERNAL_PCRE2=ON",
        # fp32 GEMM 用 Google 的 ruy（纯 C++，源码编译）。
        # 不选 OpenBLAS/MKL：它们只有预编译二进制，要么联网下载、
        # 要么依赖 vcpkg，都违反「完全离线、不引入预编译依赖」的约束。
        "-DUSE_RUY_SGEMM=ON",
        # 注意：这里**不能**用 -DCMAKE_CXX_FLAGS 来静音 C4068 ——
        # marian-dev/CMakeLists.txt 会无条件覆盖 CMAKE_CXX_FLAGS，
        # 传进去的开关会被直接丢弃。改在 patch_sources() 里改源码。
    ]
    if rc:
        cfg.append(cfg_rc)
    if not run(cfg, env, SRC, "cmake configure"):
        return 1

    if args.configure_only:
        log("")
        log("[OK] configure 完成（--configure-only）")
        return 0

    # ── build ──
    log("")
    log("[2/2] build")
    # 并行度**故意压得很低**。
    #
    # 用户实测反馈：满载编译导致同机的腾讯会议 / Zoom **双双闪退**。
    # 音视频对调度延迟极其敏感，一旦 CPU 被编译占满就会掉帧丢包甚至崩。
    # 所以这里取「核数 ÷ 8」，20 核机也只有 2~3 个 cl.exe，
    # 保证系统随时有余量调度实时进程。
    #
    # 另外 marian 的 Release flags 带 /MP（CMakeLists.txt 第 229 行），
    # 每个 cl.exe 内部还会再 fork 多个编译进程 → 实际并发是**乘积**，
    # 绝不能按核数来设。
    #
    # 嫌慢就显式传 -j N，但要自己权衡对会议软件的影响。
    if args.jobs > 0:
        jobs = args.jobs
    else:
        ncpu = os.cpu_count() or 4
        jobs = max(2, min(4, ncpu // 8))
        log(f"[JOBS] 自动限流到 {jobs}（{ncpu} 核机只给编译 {jobs} 个并行，"
            f"避免抢占导致会议软件闪退；/MP 还会再放大实际并发）")
    bld = [cmake, "--build", str(BUILD), "--target", args.target]
    if ninja:
        bld += ["--parallel", str(jobs)]
    if not run(bld, env, SRC, f"构建 {args.target}（{jobs} 并行）"):
        return 1

    # ── 找产物 ──
    # LIB_NAME 可能是 ninja 目标路径（如 src/translator/bergamot-translator.lib），
    # 产物匹配只取最后一段文件名（bergamot-translator）。
    log("")
    log("-" * 62)
    lib_stem = LIB_NAME.rsplit("/", 1)[-1].removesuffix(".lib")
    cands = list(BUILD.rglob(f"*{lib_stem}.lib")) + \
        list(BUILD.rglob(f"lib{lib_stem}.a"))
    if not cands:
        log(f"[FAIL] 没找到 {lib_stem} 产物")
        return 1
    for c in cands:
        size = c.stat().st_size / 1024 / 1024
        log(f"[OK] {c}  ({size:.1f} MB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())