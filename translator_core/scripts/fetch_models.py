#!/usr/bin/env python3
"""下载模型权重（ASR + MT）。

## 为什么需要这个脚本

模型的下载地址、文件名、路径结构都是**易变信息**：
- sherpa-onnx 的模型会换版本、换文件名
- Firefox 的模型托管在 Remote Settings CDN，附件 URL 每次部署都变

所以全部登记在 `models/sources.json`，代码不硬编码 URL。

## MT 的特殊处理

Bergamot 的 en→zh 模型**不硬编码附件 URL**，而是：

1. 查 Firefox Remote Settings 的记录 API
   `https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models/records`
2. 过滤 `fromLang == en` 且 `toLang == zh-Hans`
3. 拼 `https://firefox-settings-attachments.cdn.mozilla.net/` + `attachment.location`

这样 Firefox 换 CDN 或换模型版本都不用改代码 —— 记录 API 始终指向当前有效的附件。

> 注意：中文用 BCP 47 的 `zh-Hans`（简体），不是 `zh`。

## 离线约束

本脚本是**准备阶段**用的，允许联网。下载完成后，
交付物运行时完全离线（三层门禁 `scripts/check_offline.py` 保证）。

用法：
    python scripts/fetch_models.py                 # 下载缺失的
    python scripts/fetch_models.py --force         # 强制重下
    python scripts/fetch_models.py --check         # 只检查，不下载
    python scripts/fetch_models.py --only mt       # 只下 MT

退出码：0 成功 / 1 失败
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tarfile
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MODELS = REPO / "models"
SOURCES = MODELS / "sources.json"
ASR_DIR = MODELS / "asr"
MT_DIR = MODELS / "mt"

# 下载超时（秒）—— 模型 42 MB，30 秒足够慢的网
TIMEOUT = 60
CHUNK = 256 * 1024


def load_sources() -> dict:
    if not SOURCES.exists():
        print(f"[FAIL] 找不到 {SOURCES}", file=sys.stderr)
        sys.exit(1)
    with SOURCES.open(encoding="utf-8") as f:
        return json.load(f)


def human(n: int) -> str:
    return f"{n / 1024 / 1024:.1f} MB"


def download(url: str, dest: Path, label: str) -> bool:
    """下载到 dest（临时文件 + 原子改名，避免半截文件）。"""
    tmp = dest.with_suffix(dest.suffix + ".part")
    try:
        print(f"    ↓ {label}")
        req = urllib.request.Request(
            url, headers={"User-Agent": "Translator-model-fetcher/1.0"}
        )
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            total = int(r.headers.get("Content-Length", "0"))
            done = 0
            last_pct = -1
            with tmp.open("wb") as f:
                while True:
                    buf = r.read(CHUNK)
                    if not buf:
                        break
                    f.write(buf)
                    done += len(buf)
                    if total:
                        pct = done * 100 // total
                        # 每 10% 打一次 —— 非 TTY 下 \r 会刷屏
                        if pct // 10 != last_pct // 10:
                            last_pct = pct
                            print(f"      {pct:3d}%  {human(done)}/{human(total)}",
                                  flush=True)
        if total and done != total:
            print(f"\n    ✗ 大小不符：{done} != {total}")
            tmp.unlink(missing_ok=True)
            return False
        os.replace(tmp, dest)
        print(f"    ✓ {label}  {human(done)}")
        return True
    except urllib.error.HTTPError as e:
        print(f"\n    ✗ HTTP {e.code} {url}")
        tmp.unlink(missing_ok=True)
        return False
    except Exception as e:  # noqa: BLE001
        print(f"\n    ✗ {e}")
        tmp.unlink(missing_ok=True)
        return False


# ── ASR：单个 tar.bz2，解压出 4 个 onnx ──────────────────

def fetch_asr(cfg: dict, force: bool, check_only: bool) -> bool:
    name = cfg["name"]
    target_dir = ASR_DIR / name
    files = cfg["files"]

    missing = [k for k, v in files.items() if not (target_dir / v).exists()]
    if not missing and not force:
        print(f"[ASR] 已就绪 → {target_dir}")
        return True
    if check_only:
        print(f"[ASR] 缺失: {', '.join(missing) or '（无）'}")
        return not missing

    print(f"[ASR] {name}")
    ASR_DIR.mkdir(parents=True, exist_ok=True)
    archive = ASR_DIR / f"{name}{Path(cfg['urls'][0]).suffix}"
    if force and archive.exists():
        archive.unlink()
    if not archive.exists():
        # 多源回退：GitHub release 在部分网络环境被代理阻断
        ok = False
        for i, url in enumerate(cfg["urls"], 1):
            print(f"    源 {i}/{len(cfg['urls'])}: {url.split('/')[2]}")
            if download(url, archive, f"{name}（压缩包）"):
                ok = True
                break
            print(f"    ✗ 该源不可用，尝试下一个")
        if not ok:
            print("    ✗ 所有源都失败")
            return False

    print(f"    解压 {archive.name}…")
    try:
        with tarfile.open(archive, "r:bz2") as tf:
            # 只取需要的 4 个文件，避免解压出 test_wavs 等无关内容
            wanted = set(files.values())
            members = [m for m in tf.getmembers()
                       if Path(m.name).name in wanted]
            if not members:
                print("    ✗ 压缩包里没找到预期文件")
                return False
            for m in members:
                m.name = Path(m.name).name  # 扁平化
            # filter='data'：Python 3.14 起默认会过滤 tar 内容，
            # 不显式指定会告警；3.12 以下不认这个参数，故做兼容。
            try:
                tf.extractall(target_dir, members=members, filter="data")
            except TypeError:
                tf.extractall(target_dir, members=members)
    except Exception as e:  # noqa: BLE001
        print(f"    ✗ 解压失败: {e}")
        return False

    archive.unlink(missing_ok=True)
    ok = True
    for key, fn in files.items():
        p = target_dir / fn
        if p.exists():
            print(f"    ✓ {key:8s} {fn}  {human(p.stat().st_size)}")
        else:
            print(f"    ✗ {key:8s} {fn} 缺失")
            ok = False
    return ok


# ── MT：查记录 API → 逐个下载 ────────────────────────────

def resolve_mt_files(cfg: dict) -> dict[str, str]:
    """返回 {文件名: 完整 URL}。

    记录里同一文件名有多个变体（production / dev / beta，共享 filename
    但 location 不同）—— 按文件名去重，取第一个匹配的，避免重复下载。
    """
    api = cfg["records_api"]
    base = cfg["attachment_base"]
    want = set(cfg["files"])

    print(f"    查记录 API…")
    req = urllib.request.Request(api, headers={"User-Agent": "Translator/1.0"})
    with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
        body = json.load(r)

    found: dict[str, str] = {}
    for rec in body.get("data", []):
        if rec.get("fromLang") != cfg["from_lang"]:
            continue
        if rec.get("toLang") != cfg["to_lang"]:
            continue
        att = rec.get("attachment") or {}
        loc = att.get("location")
        if not loc:
            continue
        fname = rec.get("name") or att.get("filename") or os.path.basename(loc)
        if fname in want and fname not in found:
            found[fname] = f"{base}/{loc}"

    missing = want - set(found)
    if missing:
        print(f"    ✗ 记录里没有: {', '.join(sorted(missing))}")
    return found


def fetch_mt(cfg: dict, force: bool, check_only: bool) -> bool:
    MT_DIR.mkdir(parents=True, exist_ok=True)
    want = cfg["files"]

    missing = [f for f in want if not (MT_DIR / f).exists()]
    if not missing and not force:
        print(f"[MT] 已就绪 → {MT_DIR}")
        return True
    if check_only:
        print(f"[MT] 缺失: {', '.join(missing) or '（无）'}")
        return not missing

    print(f"[MT] {cfg['from_lang']} → {cfg['to_lang']}")
    urls = resolve_mt_files(cfg)
    if not urls:
        return False

    ok = True
    for fname in want:
        dest = MT_DIR / fname
        if dest.exists() and not force:
            print(f"    ✓ {fname}  {human(dest.stat().st_size)}（已存在）")
            continue
        url = urls.get(fname)
        if not url:
            print(f"    ✗ {fname}  无下载地址")
            ok = False
            continue
        if not download(url, dest, fname):
            ok = False
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description="下载模型权重")
    ap.add_argument("--force", action="store_true", help="强制重下已存在的")
    ap.add_argument("--check", action="store_true", help="只检查，不下载")
    ap.add_argument("--only", choices=["asr", "mt"], help="只处理其中一类")
    args = ap.parse_args()

    src = load_sources()
    print("=" * 58)
    print("模型权重获取")
    print("（准备阶段允许联网；交付物运行时完全离线）")
    print("=" * 58)

    ok = True
    if args.only in (None, "asr"):
        ok &= fetch_asr(src["asr"], args.force, args.check)
        print()
    if args.only in (None, "mt"):
        ok &= fetch_mt(src["mt"], args.force, args.check)
        print()

    print("-" * 58)
    if ok:
        print("[PASS] 模型就绪")
        return 0
    print("[FAIL] 模型获取未完成")
    return 1


if __name__ == "__main__":
    sys.exit(main())
