#!/usr/bin/env python3
"""把 sentencepiece 内置的 protobuf-lite 隔离到独立命名空间。

## 为什么需要

sherpa-onnx 的 static 库自带一份 protobuf-lite（rlib 内），与 Bergamot
依赖树里 sentencepiece 的那份**符号完全相同**（都是
`google::protobuf::internal::*`）→ 链接期 LNK2005 一整屏。

`/FORCE:MULTIPLE` 虽能绕开重复定义检查，但运行时会崩 ——
链接器可能选中 sherpa 那份**精简** protobuf（缺 Arena/ExtensionSet），
而 sentencepiece 读 .spm 词表需要完整版，ORT 初始化时 C++ 异常穿过
Rust 帧 → 「fatal runtime error: Rust cannot catch foreign exceptions」。

真正的解法：让两份 protobuf **符号名不同**。sherpa 已经这么做了
（它的 sentencepiece 符号是 `sentencepiece::Ssentencepiece`，
刻意加了 `S` 前缀避免撞名），我们照做一份即可。

## 做法

内置 protobuf-lite 这个老版本没有 `GOOGLE_PROTOBUF_NAMESPACE` 宏
（那是 protobuf 3.x 后期才加的），命名空间是硬编码的
`namespace google {`。所以直接做源码级替换：

- `namespace google` → `namespace spm_google`
- `google::protobuf` → `spm_google::protobuf`

protobuf-lite 内部只在 `google::protobuf` 下定义东西，
`google::` 没有别的用法，所以替换是安全的。

## 幂等

已改过的文件会被跳过（检测 `spm_google` 标记），
重复执行不会二次替换。
"""

import io
import os
import re
import sys

# 命名空间映射
# ⚠️ 必须用正则要求 `namespace google` 后面跟 `{`：
#    否则会误伤 `namespace google_opensource {`（protobuf 里的 upb 嵌套），
#    导致定义处变成 spm_google_opensource 而引用处仍是 google_opensource
#    → error C2039「google_opensource 不是 spm_google::upb 的成员」。
NS_RE = re.compile(r"\bnamespace google\b(?=\s*\{)")
OLD_QUAL = "google::protobuf"
NEW_QUAL = "spm_google::protobuf"
MARKER = "spm_google"

# 需要处理的扩展名
EXTS = (".h", ".cc", ".inc", ".c", ".hpp", ".cpp")


def revert_file(path: str) -> bool:
    """把之前改坏的改动还原（spm_google → google）。

    反向替换是安全的：spm_google 只由本脚本产生，
    `namespace spm_google_opensource` 会正确还原成
    `namespace google_opensource`。
    """
    try:
        with io.open(path, encoding="utf-8") as f:
            src = f.read()
    except (UnicodeDecodeError, OSError):
        return False
    if MARKER not in src:
        return False
    out = src.replace("spm_google", "google")
    with io.open(path, "w", encoding="utf-8", newline="") as f:
        f.write(out)
    return True


def patch_file(path: str) -> bool:
    """改一个文件。返回是否发生了修改。"""
    try:
        with io.open(path, encoding="utf-8") as f:
            src = f.read()
    except (UnicodeDecodeError, OSError):
        return False

    if MARKER in src:
        return False  # 已改过

    if "namespace google" not in src and OLD_QUAL not in src:
        return False

    out = NS_RE.sub("namespace spm_google", src)
    out = out.replace(OLD_QUAL, NEW_QUAL)
    # 兜底：protobuf-lite 里少量 `google::` 裸引用（不带 protobuf）
    out = re.sub(r"\bgoogle::(?!protobuf)", "spm_google::", out)

    with io.open(path, "w", encoding="utf-8", newline="") as f:
        f.write(out)
    return True


def walk_files(root: str):
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            if name.endswith(EXTS):
                yield os.path.join(dirpath, name)


def main() -> int:
    args = sys.argv[1:]
    revert = False
    if args and args[0] == "--revert":
        revert = True
        args = args[1:]
    if not args:
        print(__doc__)
        return 2
    total = 0
    for root in args:
        if not os.path.isdir(root):
            print(f"[跳过] 不是目录: {root}")
            continue
        n = 0
        for p in walk_files(root):
            if (revert_file(p) if revert else patch_file(p)):
                n += 1
        verb = "还原" if revert else "改写"
        print(f"[OK] {root} → {verb} {n} 个文件")
        total += n
    print(f"[合计] {verb} {total} 个文件")
    return 0


if __name__ == "__main__":
    sys.exit(main())
