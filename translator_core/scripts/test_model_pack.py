#!/usr/bin/env python3
"""模型包脚本自测：验证 SHA-256 能检测篡改。

用于 CI —— 若篡改检测失效，说明打包流程有严重缺陷。
"""

from __future__ import annotations

import json
import subprocess
import sys
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PY = sys.executable
PACK = REPO / "dist" / "minimal.dshm"
TAMPERED = REPO / "dist" / "tampered.dshm"


def run(cmd: list[str]) -> tuple[int, str]:
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=REPO)
    return r.returncode, r.stdout + r.stderr


def main() -> int:
    if not PACK.exists():
        print(f"[SKIP] 包不存在，跳过: {PACK}")
        print("       先运行 build_model_pack.py pack")
        return 0

    # 1. 正常包必须通过
    rc, out = run([PY, "scripts/build_model_pack.py", "verify", str(PACK)])
    if rc != 0:
        print(f"[FAIL] 正常包校验应通过，实际失败:\n{out}")
        return 1
    print("[PASS] 正常包校验通过")

    # 2. 篡改包必须被拒绝
    with zipfile.ZipFile(PACK) as zin:
        with zipfile.ZipFile(TAMPERED, "w") as zout:
            for name in zin.namelist():
                data = zin.read(name)
                # 改动一个非 manifest 文件，manifest 里的 sha256 不变
                if name.endswith("tokens.txt"):
                    data = b"TAMPERED CONTENT"
                zout.writestr(name, data)

    rc, out = run([PY, "scripts/build_model_pack.py", "verify", str(TAMPERED)])
    if rc == 0:
        print("[FAIL] 篡改包应被拒绝，但校验通过了 —— SHA-256 检测失效！")
        return 1
    if "校验不通过" not in out:
        print(f"[FAIL] 篡改包被拒绝，但错误信息不明确:\n{out}")
        return 1
    print("[PASS] 篡改包被正确拒绝")

    # 3. manifest 结构完整性
    with zipfile.ZipFile(PACK) as z:
        m = json.loads(z.read("manifest.json"))
    required = {"pack_version", "name", "version", "files", "licenses"}
    missing = required - set(m.keys())
    if missing:
        print(f"[FAIL] manifest 缺字段: {missing}")
        return 1
    if not m["licenses"]:
        print("[FAIL] manifest 未声明许可证 —— 分发会有合规风险")
        return 1
    print(f"[PASS] manifest 结构完整（{len(m['files'])} 文件，{len(m['licenses'])} 项许可）")

    # 4. 每个文件都有 sha256
    for f in m["files"]:
        if len(f.get("sha256", "")) != 64:
            print(f"[FAIL] 文件缺 SHA-256: {f.get('path')}")
            return 1
    print("[PASS] 所有文件均带 SHA-256")

    TAMPERED.unlink(missing_ok=True)
    print("\n[PASS] 模型包脚本自测全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
