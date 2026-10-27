#!/usr/bin/env python3
"""获取 Bergamot C++ 源码（含 submodule）。

## 为什么不用 git clone

本机的网络环境里 **GitHub 的 git 协议与 release 附件都被代理阻断**：

    $ git clone --depth 1 https://github.com/mozilla/bergamot-translator.git
    fatal: unable to access '...': CONNECT tunnel failed, response 502

    $ curl -L https://github.com/k2-fsa/sherpa-onnx/releases/download/...
    HTTP=000

但 `codeload.github.com`（源码打包端点，不走 git 协议）是通的：

    $ curl -L https://codeload.github.com/mozilla/bergamot-translator/tar.gz/refs/heads/main
    HTTP=200

所以本脚本用 codeload 逐个下载仓库 tar.gz，**递归处理 submodule**
（bergamot-translator 有 marian-dev / ssplit-cpp 两个必需 submodule）。

## submodule 清单

| 路径 | 仓库 | 是否必需 |
|---|---|---|
| `3rd_party/marian-dev` | browsermt/marian-dev | ✅ 核心推理引擎 |
| `3rd_party/ssplit-cpp` | browsermt/ssplit-cpp | ✅ 句子切分 |
| `3rd_party/pybind11` | pybind/pybind11 | ❌ 只用于 Python 绑定 |
| `bergamot-translator-tests` | browsermt/bergamot-translator-tests | ❌ 只用于测试 |

默认只下必需的两个（`--all` 可下全部）。

用法：
    python scripts/fetch_bergamot.py
    python scripts/fetch_bergamot.py --all
    python scripts/fetch_bergamot.py --check
"""

from __future__ import annotations

import argparse
import io
import re
import shutil
import sys
import tarfile
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _archive import extract_any, extract_bytes, find_7z  # noqa: E402

REPO = Path(__file__).resolve().parent.parent.parent / "translator_core"
DEST = REPO / "third_party" / "bergamot-translator"

CODELOAD = "https://codeload.github.com/{repo}/tar.gz/refs/heads/{branch}"
TIMEOUT = 90

# 主仓库
MAIN_REPO = "mozilla/bergamot-translator"
MAIN_BRANCH = "main"

# 必需的 submodule（路径 → 仓库名），相对仓库根
REQUIRED_SUBMODULES = {
    "3rd_party/marian-dev": "browsermt/marian-dev",
    "3rd_party/ssplit-cpp": "browsermt/ssplit-cpp",
}
OPTIONAL_SUBMODULES = {
    "3rd_party/pybind11": "pybind/pybind11",
    "bergamot-translator-tests": "browsermt/bergamot-translator-tests",
}

# marian-dev 内部的 submodule：CPU 构建只需这 3 个
#   sentencepiece —— 分词
#   intgemm      —— int8 GEMM（我们的模型就是 intgemm.alphas.bin）
#   simd_utils   —— SIMD 加速
# 不需要：nccl / fbgemm（GPU）、onnxjs、examples、regression-tests
MARIAN_REQUIRED = {
    "src/3rd_party/sentencepiece": "browsermt/sentencepiece",
    "src/3rd_party/intgemm": "kpu/intgemm",
    "src/3rd_party/simd_utils": "browsermt/simd_utils",
}


def log(msg: str) -> None:
    print(msg, flush=True)


def fetch_tarball(repo: str, branch: str) -> bytes | None:
    """下载 tar.gz。分支不存在时试 master。"""
    for br in (branch, "master", "main"):
        url = CODELOAD.format(repo=repo, branch=br)
        try:
            req = urllib.request.Request(
                url, headers={"User-Agent": "Translator/1.0"}
            )
            with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
                data = r.read()
            if len(data) > 1000:
                log(f"    ↓ {repo}@{br}  {len(data) / 1024:.0f} KB")
                return data
        except urllib.error.HTTPError as e:
            if e.code != 404:
                log(f"    ✗ {repo}@{br}  HTTP {e.code}")
        except Exception as e:  # noqa: BLE001
            log(f"    ✗ {repo}@{br}  {e}")
    return None


def extract_strip(data: bytes, dest: Path) -> bool:
    """从内存字节解压到 dest（剥掉顶层目录）。"""
    return extract_bytes(data, dest)


def parse_gitmodules(text: str) -> list[tuple[str, str]]:
    """解析 .gitmodules → [(path, url)]"""
    out: list[tuple[str, str]] = []
    path = url = None
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("path"):
            path = line.split("=", 1)[1].strip()
        elif line.startswith("url"):
            url = line.split("=", 1)[1].strip()
        elif line.startswith("[submodule"):
            if path and url:
                out.append((path, url))
            path = url = None
    if path and url:
        out.append((path, url))
    return out


def repo_from_url(url: str) -> str:
    """https://github.com/owner/repo(.git) → owner/repo"""
    m = re.search(r"github\.com[/:]([^/]+/[^/]+?)(?:\.git)?$", url)
    return m.group(1) if m else ""


def main() -> int:
    ap = argparse.ArgumentParser(description="获取 Bergamot C++ 源码")
    ap.add_argument("--all", action="store_true", help="连可选 submodule 一起下")
    ap.add_argument("--check", action="store_true", help="只检查，不下载")
    ap.add_argument("--force", action="store_true", help="已存在也重新下载")
    args = ap.parse_args()

    log("=" * 62)
    log("Bergamot C++ 源码获取（走 codeload，绕开被阻断的 git 协议）")
    log("=" * 62)

    need = dict(REQUIRED_SUBMODULES)
    if args.all:
        need.update(OPTIONAL_SUBMODULES)

    if args.check:
        ok = True
        for rel, repo in need.items():
            p = DEST / rel
            n = sum(1 for _ in p.rglob("*")) if p.exists() else 0
            print(f"  {'✓' if n > 3 else '✗'} {rel:32s} ({n} 个文件)")
            ok &= n > 3
        print()
        print("[PASS] 源码齐备" if ok else "[FAIL] 源码不全")
        return 0 if ok else 1

    # ── 主仓库 ──
    if DEST.exists() and not args.force:
        log(f"[SKIP] {DEST} 已存在（--force 可重下）")
    else:
        log(f"[MAIN] {MAIN_REPO}")
        data = fetch_tarball(MAIN_REPO, MAIN_BRANCH)
        if data is None:
            log("[FAIL] 主仓库下载失败")
            return 1
        if DEST.exists():
            shutil.rmtree(DEST)
        if not extract_strip(data, DEST):
            log("[FAIL] 解压失败")
            return 1
        log(f"    ✓ → {DEST}")

    # ── submodule ──
    gm = DEST / ".gitmodules"
    if not gm.exists():
        log("[WARN] 没有 .gitmodules，跳过 submodule")
        return 0

    subs = parse_gitmodules(gm.read_text(encoding="utf-8"))
    log("")
    log(f"[SUB] 共 {len(subs)} 个 submodule")

    for rel, url in subs:
        repo = repo_from_url(url)
        if not repo:
            log(f"    ✗ 无法解析仓库名: {url}")
            continue
        if rel not in need:
            log(f"    - 跳过（可选）{rel}")
            continue

        target = DEST / rel
        if target.exists() and any(target.iterdir()) and not args.force:
            log(f"    ✓ 已存在 {rel}")
            continue

        log(f"    ↓ {rel}  ← {repo}")
        data = fetch_tarball(repo, "main")
        if data is None:
            log(f"    ✗ {rel} 下载失败")
            continue
        if target.exists():
            shutil.rmtree(target)
        if extract_strip(data, target):
            n = sum(1 for _ in target.rglob("*"))
            log(f"    ✓ {rel}  ({n} 个文件)")
        else:
            log(f"    ✗ {rel} 解压失败")

    # ── marian-dev 内部的 submodule ──
    marian = DEST / "3rd_party" / "marian-dev"
    if marian.exists():
        log("")
        log("[SUB2] marian-dev 的 submodule（CPU 构建必需项）")
        marian_gm = marian / ".gitmodules"
        if marian_gm.exists():
            for rel, url in parse_gitmodules(marian_gm.read_text(encoding="utf-8")):
                if rel not in MARIAN_REQUIRED:
                    log(f"    - 跳过（不需要）{rel}")
                    continue
                repo = repo_from_url(url)
                target = marian / rel
                if target.exists() and any(target.iterdir()) and not args.force:
                    log(f"    ✓ 已存在 {rel}")
                    continue
                log(f"    ↓ {rel}  ← {repo}")
                data = fetch_tarball(repo, "main")
                if data is None:
                    log(f"    ✗ {rel} 下载失败")
                    continue
                if target.exists():
                    shutil.rmtree(target)
                if extract_strip(data, target):
                    n = sum(1 for _ in target.rglob("*"))
                    log(f"    ✓ {rel}  ({n} 个文件)")
                else:
                    log(f"    ✗ {rel} 解压失败")

    # ── 校验 ──
    log("")
    log("-" * 62)
    ok = True
    for rel in REQUIRED_SUBMODULES:
        p = DEST / rel
        n = sum(1 for _ in p.rglob("*")) if p.exists() else 0
        good = n > 3
        log(f"  {'✓' if good else '✗'} {rel}  ({n} 个文件)")
        ok &= good
    marian = DEST / "3rd_party" / "marian-dev"
    for rel in MARIAN_REQUIRED:
        p = marian / rel
        n = sum(1 for _ in p.rglob("*")) if p.exists() else 0
        good = n > 3
        log(f"  {'✓' if good else '✗'} marian-dev/{rel}  ({n} 个文件)")
        ok &= good
    if ok:
        log("[PASS] Bergamot 源码齐备，可编译静态库")
        return 0
    log("[FAIL] 必需 submodule 缺失")
    return 1


if __name__ == "__main__":
    sys.exit(main())
