#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""出厂产物验证门禁（音频无关）。

用途：在 package.sh 覆盖 last_good / outputs 之前，确认当前产物
（真实的 translator_core.dll + 模型）能够：
  1. 被加载、符号可解析；
  2. 创建会话；
  3. 自检 5 步中 lib / asr / mt / glossary 全部 ok
     （asr / mt 的 ok 取决于模型目录结构有效——
      即真实模型随产物正确打包；与有无音频设备无关）；
  4. capture 步骤仅作警告（无音频设备的机器上本就为 false）。

不依赖显示器、不依赖麦克风、不发起任何网络请求。
退出码 0 = 通过（可覆盖 last_good）；非 0 = 未通过（保持 last_good 不动）。

用法：
  python verify_product.py <DLL目录> [模型根目录]
"""

import ctypes as C
import json
import os
import sys
from pathlib import Path

PY = r"C:\Users\Admin\.workbuddy\binaries\python\versions\3.13.12\python.exe"


def find_asr_dir(root: Path) -> "Path | None":
    """在 root/models/asr 下找含 encoder*.onnx 的目录。"""
    asr_root = root / "models" / "asr"
    if asr_root.is_dir():
        for d in sorted(asr_root.iterdir()):
            if d.is_dir() and any(p.name.startswith("encoder") and p.suffix == ".onnx"
                                  for p in d.iterdir()):
                return d
    return None


def find_mt_dir(root: Path) -> "Path | None":
    """在 root/models/mt 下找含 config.yml 的目录。"""
    mt_root = root / "models" / "mt"
    if mt_root.is_dir():
        if (mt_root / "config.yml").exists():
            return mt_root
        for d in sorted(mt_root.iterdir()):
            if d.is_dir() and (d / "config.yml").exists():
                return d
    return None


def main() -> int:
    if len(sys.argv) < 2:
        print("[FAIL] 用法: verify_product.py <DLL目录> [模型根目录]")
        return 2
    dll_dir = Path(sys.argv[1]).resolve()
    dll = dll_dir / "translator_core.dll"
    if not dll.exists():
        print(f"[FAIL] 找不到 {dll}")
        return 2

    model_root = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else dll_dir
    asr_dir = find_asr_dir(model_root)
    mt_dir = find_mt_dir(model_root)
    if not asr_dir or not mt_dir:
        print(f"[FAIL] 模型目录探测失败: asr={asr_dir} mt={mt_dir}")
        return 2

    os.add_dll_directory(str(dll_dir))
    try:
        lib = C.CDLL(str(dll))
    except OSError as e:
        print(f"[FAIL] 加载 DLL 失败: {e}")
        return 2
    print(f"[INFO] 已加载 {dll}")

    lib.translator_version.restype = C.c_void_p
    lib.translator_free_string.argtypes = [C.c_void_p]
    lib.translator_free_string.restype = None
    lib.translator_session_create.restype = C.c_void_p
    lib.translator_session_create.argtypes = [C.c_char_p, C.c_int, C.c_int]
    lib.translator_selfcheck_json.restype = C.c_void_p
    lib.translator_selfcheck_json.argtypes = [C.c_void_p]
    lib.translator_session_free.argtypes = [C.c_void_p]
    # 会话采集能力（UI 采集状态卡的数据源）
    lib.translator_session_capture_capability.restype = C.c_void_p
    lib.translator_session_capture_capability.argtypes = [C.c_void_p]

    def take_str(p):
        if not p:
            return ""
        s = C.cast(p, C.c_char_p).value.decode("utf-8", "replace")
        lib.translator_free_string(p)
        return s

    ver = take_str(lib.translator_version())
    if not ver:
        print("[FAIL] 版本字符串为空（DLL 异常）")
        return 2
    print(f"[PASS] 库加载 + 版本: {ver}")

    cfg = {
        "backend": "auto",
        "profile": "stable",
        "asr_model_dir": str(asr_dir),
        "mt_model_dir": str(mt_dir),
        # 显式配置双声源（与 UI 默认一致），这样 session 会真的建两个采集器，
        # 采集能力 FFI 才有意义（sources 为空会走"兼容旧调用方"分支只建一个）。
        "sources": [
            {"kind": "systemAudio", "speaker": "对方"},
            {"kind": "microphone", "speaker": "我"},
        ],
        "transcript_dir": str(Path(os.environ.get("TEMP", "/tmp")) / "translator-verify"),
    }
    h = lib.translator_session_create(
        C.c_char_p(json.dumps(cfg).encode("utf-8")), 0, 0)
    if not h:
        print("[FAIL] 无法创建会话（DLL 初始化失败）")
        return 2
    print("[PASS] session_create")

    sc = json.loads(take_str(lib.translator_selfcheck_json(h)) or "{}")
    steps = {s.get("name"): s for s in sc.get("steps", [])}
    print("[INFO] 自检步骤:")
    for name, s in steps.items():
        print(f"        {name}: ok={s.get('ok')}  {s.get('detail','')}")

    required = ["lib", "asr", "mt", "glossary"]
    ok_all = True
    for name in required:
        s = steps.get(name)
        if not s or not s.get("ok"):
            print(f"[FAIL] 自检步骤未通过: {name} -> {s}")
            ok_all = False
    cap = steps.get("capture")
    if not cap or not cap.get("ok"):
        print(f"[WARN] capture 未就绪（无音频设备属正常，不影响出厂）: "
              f"{cap.get('detail') if cap else '-'}")

    # ── 会话采集能力（UI「采集可用性」显示的数据源）─────────────
    # 这个 FFI 是 UI 采集状态卡的唯一依据，必须存在且返回合法 JSON，
    # 否则 UI 会退回到「采集句柄未创建」的误报路径。
    try:
        cap_raw = take_str(lib.translator_session_capture_capability(h))
    except AttributeError:
        print("[FAIL] DLL 缺少 translator_session_capture_capability 导出")
        return 2
    if not cap_raw:
        print("[FAIL] translator_session_capture_capability 返回空")
        ok_all = False
    else:
        try:
            cj = json.loads(cap_raw)
        except json.JSONDecodeError as e:
            print(f"[FAIL] 采集能力 JSON 非法: {e} -> {cap_raw[:120]}")
            ok_all = False
        else:
            kind = cj.get("kind")
            srcs = cj.get("sources") or []
            print(f"[INFO] 采集能力: kind={kind} sources={srcs} "
                  f"reason={cj.get('reason') or '-'}")
            if kind not in ("systemAudio", "microphone", "unavailable"):
                print(f"[FAIL] 采集能力 kind 非法: {kind}")
                ok_all = False
            elif not srcs:
                print("[FAIL] 采集能力未列出任何声源")
                ok_all = False
            else:
                print("[PASS] 会话采集能力可用")

    lib.translator_session_free(h)
    lib.translator_session_free(None)  # 幂等
    print("[PASS] session_free 幂等")

    if not ok_all:
        print("[结果] 未通过 —— 保持 last_good 不动")
        return 1
    print("[结果] 通过 —— 可覆盖 last_good")
    return 0


if __name__ == "__main__":
    sys.exit(main())
