#!/usr/bin/env python3
"""把 onnxruntime.dll 复制到目标目录（绕过 Inno Setup 的 MoveFile 限制）。

## 为什么需要这个

Windows 11 的 AI 组件（Microsoft.AIFabric / Client.CoreAI）会**按文件名**
自动扫描并加载 `onnxruntime.dll`。Inno Setup 解压后要把临时文件
MoveFile 到 `{app}\onnxruntime.dll`，此时该 DLL 已被系统组件加载占用 →
MoveFile 失败（错误码 5 = ACCESS_DENIED）→ 安装中止。

实测结论（同一安装器、同一内容、同一目录，唯一差别是目标文件名）：
- 目标名 `onnxruntime.dll` → 失败
- 目标名 `ort_runtime.dll`  → 成功
- 但 sherpa-onnx-c-api.dll **静态导入** `onnxruntime.dll`，名字不能改

而用 Python 直接 `shutil.copy2` / `MoveFileEx` 落到该文件名是**成功**的
（系统只在文件被加载后才锁定，Copy 走的是写句柄而非移动句柄）。

所以：安装器把 payload 装到 `{tmp}`，再用本脚本复制到最终位置。

用法：
    python copy_ort.py <源文件> <目标文件>
退出码 0 = 成功，1 = 失败
"""

import ctypes
import os
import shutil
import sys
import time

# MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED
MOVEFILE_REPLACE_EXISTING = 0x1
MOVEFILE_COPY_ALLOWED = 0x2


def _move_file_ex(src: str, dst: str) -> bool:
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.MoveFileExW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_uint,
    ]
    k32.MoveFileExW.restype = ctypes.c_int
    return bool(
        k32.MoveFileExW(
            src, dst, MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED
        )
    )


def copy_ort(src: str, dst: str) -> bool:
    """把 src 复制到 dst。先试原地复制，失败再走 MoveFileEx。"""
    if not os.path.isfile(src):
        print(f"[copy_ort] 源文件不存在: {src}", file=sys.stderr)
        return False

    os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)

    # 首选：直接复制（走写句柄，绕开移动句柄被锁）
    try:
        shutil.copy2(src, dst)
        if os.path.getsize(dst) == os.path.getsize(src):
            print(f"[copy_ort] 复制成功: {dst}")
            return True
    except Exception as e:  # noqa: BLE001
        print(f"[copy_ort] 直接复制失败({e})，改用 MoveFileEx", file=sys.stderr)

    # 退路：移动
    tmp = dst + ".moving"
    try:
        if os.path.exists(tmp):
            os.remove(tmp)
    except OSError:
        pass
    if _move_file_ex(src, tmp):
        try:
            if _move_file_ex(tmp, dst):
                print(f"[copy_ort] 移动成功: {dst}")
                return True
        except Exception:
            pass
    return False


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    src, dst = sys.argv[1], sys.argv[2]
    # 最多试 3 次（系统组件加载有延迟，刚落地时可能短暂被锁）
    for attempt in range(3):
        if copy_ort(src, dst):
            return 0
        time.sleep(1.0 * (attempt + 1))
    print(f"[copy_ort] 复制失败: {src} -> {dst}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
