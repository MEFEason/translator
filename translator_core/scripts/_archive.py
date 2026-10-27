#!/usr/bin/env python3
"""归档解压工具。

## 分工

| 格式 | 首选 | 理由 |
|---|---|---|
| `.tar.gz` / `.tar.bz2` | **Python tarfile** | 已实测可靠；Windows 自带 `tar` 对 tar.bz2 会报 `Cannot utime` 并 exit 2 |
| `.7z` / `.zip` | **7-Zip** | tarfile 不支持这两种 |
| 其它 | 7-Zip 兜底 | 格式多，容错好 |

7-Zip 路径：`C:\Program Files\7-Zip\7z.exe`（7-Zip 26.03 x64）。

## 7z 的两个坑

1. **嵌套归档只解一层** —— 解 `.tar.gz` 得到 `.tar` 就停了，还得再解一次 tar。
   `extract_any` 统一处理：解到没有 `.tar` 残留为止（最多 3 层）。
2. 解出来的 tar 通常带一层顶层目录（GitHub 打包惯例 `repo-main/...`），
   `strip_top` 逻辑会把它剥掉。
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

SEVENZIP_CANDIDATES = [
    Path(r"C:\Program Files\7-Zip\7z.exe"),
    Path(r"C:\Program Files (x86)\7-Zip\7z.exe"),
]


def find_7z() -> Path | None:
    """定位 7z.exe。找不到返回 None。"""
    for p in SEVENZIP_CANDIDATES:
        if p.exists():
            return p
    found = shutil.which("7z")
    return Path(found) if found else None


def _strip_top(dest: Path, tmp: Path) -> None:
    """把 tmp 里唯一顶层目录的内容移到 dest（剥掉一层）。"""
    entries = list(tmp.iterdir())
    if len(entries) == 1 and entries[0].is_dir():
        src = entries[0]
    else:
        src = tmp
    for item in list(src.iterdir()):
        target = dest / item.name
        if target.exists():
            if target.is_dir():
                shutil.rmtree(target, ignore_errors=True)
            else:
                target.unlink(missing_ok=True)
        shutil.move(str(item), str(target))


def _extract_7z_once(archive: Path, out: Path) -> bool:
    sevenz = find_7z()
    if sevenz is None:
        return False
    out.mkdir(parents=True, exist_ok=True)
    # -y 自动覆盖；-bso0 -bsp0 抑制输出与进度（避免刷屏）
    r = subprocess.run(
        [str(sevenz), "x", str(archive), f"-o{out}", "-y", "-bso0", "-bsp0"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        return False
    # 嵌套归档只解一层 → 继续解残留的 .tar（最多 3 层）
    for _ in range(3):
        tars = [f for f in out.iterdir() if f.is_file() and f.suffix == ".tar"]
        if not tars:
            break
        inner = tars[0]
        sub = out / (inner.stem + ".untar")
        sub.mkdir(exist_ok=True)
        r2 = subprocess.run(
            [str(sevenz), "x", str(inner), f"-o{sub}", "-y", "-bso0", "-bsp0"],
            capture_output=True,
            text=True,
        )
        if r2.returncode != 0:
            shutil.rmtree(sub, ignore_errors=True)
            break
        inner.unlink(missing_ok=True)
        for item in list(sub.iterdir()):
            shutil.move(str(item), str(out / item.name))
        shutil.rmtree(sub, ignore_errors=True)
    return True


def _extract_tarfile(archive: Path, dest: Path) -> bool:
    """Python tarfile 解压（剥掉顶层目录）。"""
    dest.mkdir(parents=True, exist_ok=True)
    tmp = dest.parent / (dest.name + ".tarfile_tmp")
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True, exist_ok=True)
    try:
        with tarfile.open(archive) as tf:
            members = tf.getmembers()
            if not members:
                return False
            top = members[0].name.split("/")[0]
            for m in members:
                rel = m.name[len(top) + 1:] if m.name.startswith(top) else m.name
                if not rel or rel.startswith("/") or ".." in rel.split("/"):
                    continue
                m.name = rel
            try:
                tf.extractall(tmp, members=members, filter="data")
            except TypeError:
                # Python < 3.12 不认 filter 参数
                tf.extractall(tmp, members=members)
        for item in list(tmp.iterdir()):
            shutil.move(str(item), str(dest / item.name))
        return True
    except Exception as e:  # noqa: BLE001
        print(f"      tarfile 失败: {e}", file=sys.stderr)
        return False
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def extract_any(archive: Path, dest: Path, prefer_7z: bool = False) -> bool:
    """解压任意支持的归档到 dest（剥掉顶层目录）。

    `prefer_7z` 为真时先试 7z（适合 .7z/.zip），
    否则先试 tarfile（对 .tar.* 更可靠）。
    """
    order = ("7z", "tarfile") if prefer_7z else ("tarfile", "7z")
    for how in order:
        if how == "7z":
            tmp = dest.parent / (dest.name + ".7ztmp")
            shutil.rmtree(tmp, ignore_errors=True)
            ok = _extract_7z_once(archive, tmp)
            if ok:
                dest.mkdir(parents=True, exist_ok=True)
                _strip_top(dest, tmp)
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            ok = _extract_tarfile(archive, dest)
        if ok:
            return True
    return False


def extract_bytes(data: bytes, dest: Path) -> bool:
    """从内存字节解压到 dest。"""
    tmp = dest.parent / (dest.name + ".dl")
    tmp.write_bytes(data)
    try:
        return extract_any(tmp, dest)
    finally:
        tmp.unlink(missing_ok=True)
