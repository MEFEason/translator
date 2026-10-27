#!/usr/bin/env python3
"""模型包（.dshm）构建与校验。

.dshm = zip 归档，内含 manifest.json + 模型文件 + SHA-256 校验。

设计要点（DESIGN.md §3.2）：
- **原子提交**：校验全部通过才写入目标目录，中途失败只清临时目录
- **磁盘预检**：空间不足 ≥2 倍包体积时提前拒绝
- **离线性**：本脚本只在构建机运行（联网），产物随包分发；
  应用运行期只接受本地文件导入

用法：
    # 构建模型包
    python scripts/build_model_pack.py pack \\
        --name minimal --output dist/minimal.dshm \\
        --manifest models/manifest/minimal.json

    # 校验已有包
    python scripts/build_model_pack.py verify dist/minimal.dshm
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PACK_VERSION = 1


@dataclass
class FileEntry:
    """模型包内的一个文件。"""

    path: str  # 包内相对路径
    sha256: str
    size: int


@dataclass
class Manifest:
    """模型包清单。"""

    name: str
    version: str
    description: str = ""
    files: list[FileEntry] = field(default_factory=list)
    # 许可证：分发时必须随包附 THIRD_PARTY_LICENSES
    licenses: dict[str, str] = field(default_factory=dict)
    # 该包提供的模型角色 → 目录名
    provides: dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> dict:
        return {
            "pack_version": PACK_VERSION,
            "name": self.name,
            "version": self.version,
            "description": self.description,
            "provides": self.provides,
            "licenses": self.licenses,
            "files": [
                {"path": f.path, "sha256": f.sha256, "size": f.size}
                for f in self.files
            ],
        }

    @staticmethod
    def from_dict(d: dict) -> "Manifest":
        return Manifest(
            name=d.get("name", ""),
            version=d.get("version", "0"),
            description=d.get("description", ""),
            provides=d.get("provides", {}),
            licenses=d.get("licenses", {}),
            files=[
                FileEntry(f["path"], f["sha256"], f.get("size", 0))
                for f in d.get("files", [])
            ],
        )


def sha256_of(path: Path) -> str:
    """分块计算 SHA-256，避免大模型文件撑爆内存。"""
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def human(n: int) -> str:
    for unit in ["B", "KB", "MB", "GB"]:
        if n < 1024:
            return f"{n:.1f} {unit}" if unit != "B" else f"{n} B"
        n /= 1024
    return f"{n:.1f} TB"


def cmd_pack(args: argparse.Namespace) -> int:
    """构建 .dshm 模型包。"""
    spec_path = Path(args.manifest)
    if not spec_path.exists():
        print(f"[FAIL] 清单文件不存在: {spec_path}")
        return 1

    spec = json.loads(spec_path.read_text(encoding="utf-8"))
    manifest = Manifest(
        name=spec.get("name", args.name),
        version=spec.get("version", "0.1.0"),
        description=spec.get("description", ""),
        provides=spec.get("provides", {}),
        licenses=spec.get("licenses", {}),
    )

    # 收集文件：spec["models"] 形如 [{"role":"asr","path":"models/asr/xxx", ...}]
    total = 0
    missing: list[str] = []
    for entry in spec.get("models", []):
        src = REPO / entry["path"]
        if not src.exists():
            missing.append(str(src))
            continue
        if src.is_dir():
            for f in sorted(src.rglob("*")):
                if f.is_file():
                    rel = f.relative_to(src)
                    size = f.stat().st_size
                    total += size
                    manifest.files.append(
                        FileEntry(
                            path=f"{entry['path']}/{rel.as_posix()}",
                            sha256=sha256_of(f),
                            size=size,
                        )
                    )
        else:
            size = src.stat().st_size
            total += size
            manifest.files.append(
                FileEntry(path=entry["path"], sha256=sha256_of(src), size=size)
            )

    if missing:
        print("[FAIL] 以下模型文件不存在：")
        for m in missing:
            print(f"  - {m}")
        print("\n提示：先运行 python scripts/fetch_models.py 下载权重")
        return 1

    print(f"[INFO] 文件数 {len(manifest.files)}，总体积 {human(total)}")

    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)

    # 磁盘预检（§3.2）
    try:
        free = shutil.disk_usage(out.parent).free
        need = total * 2  # zip + 解压临时
        if free < need:
            print(f"[FAIL] 磁盘空间不足：需要 {human(int(need))}，可用 {human(free)}")
            return 1
    except OSError:
        pass  # 拿不到磁盘信息不阻塞

    # 写 zip：先写临时文件再改名，避免中断产生半包
    tmp = out.with_suffix(out.suffix + ".tmp")
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED, compresslevel=5) as z:
        z.writestr("manifest.json", json.dumps(manifest.to_dict(), ensure_ascii=False, indent=2))
        for f in manifest.files:
            z.write(REPO / f.path, f.path)
    tmp.replace(out)

    digest = sha256_of(out)
    print(f"[PASS] 已生成 {out}（{human(out.stat().st_size)}）")
    print(f"       SHA-256: {digest}")
    print(f"       文件数: {len(manifest.files)}")
    if manifest.licenses:
        print(f"       许可证: {', '.join(manifest.licenses.keys())}")
        print("       分发时须附 THIRD_PARTY_LICENSES")
    return 0


def cmd_verify(args: argparse.Namespace) -> int:
    """校验已有 .dshm 包。"""
    p = Path(args.pack)
    if not p.exists():
        print(f"[FAIL] 包不存在: {p}")
        return 1

    with zipfile.ZipFile(p) as z:
        names = set(z.namelist())
        if "manifest.json" not in names:
            print("[FAIL] 缺少 manifest.json")
            return 1

        manifest = Manifest.from_dict(json.loads(z.read("manifest.json")))

        bad = 0
        for f in manifest.files:
            if f.path not in names:
                print(f"[FAIL] 缺失文件: {f.path}")
                bad += 1
                continue
            # 逐文件校验（流式，避免整包入内存）
            h = hashlib.sha256()
            with z.open(f.path) as fh:
                for block in iter(lambda: fh.read(1024 * 1024), b""):
                    h.update(block)
            if h.hexdigest() != f.sha256:
                print(f"[FAIL] 校验不通过: {f.path}")
                bad += 1

    if bad:
        print(f"\n[FAIL] {bad} 个文件有问题")
        return 1
    print(f"[PASS] {p.name} 校验通过（{len(manifest.files)} 个文件）")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="模型包构建与校验")
    sub = ap.add_subparsers(dest="cmd", required=True)

    pk = sub.add_parser("pack", help="构建模型包")
    pk.add_argument("--name", required=True, help="包名")
    pk.add_argument("--manifest", required=True, help="清单 JSON")
    pk.add_argument("--output", required=True, help="输出路径")
    pk.set_defaults(func=cmd_pack)

    vf = sub.add_parser("verify", help="校验模型包")
    vf.add_argument("pack", help="包路径")
    vf.set_defaults(func=cmd_verify)

    args = ap.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
