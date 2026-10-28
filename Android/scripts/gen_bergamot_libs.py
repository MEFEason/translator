#!/usr/bin/env python3
r"""从 CMake 的 Ninja 构建目录导出 Bergamot 静态库的**链接清单**。

## 为什么需要清单

Bergamot 的静态库是一张**依赖图**，不是一个库：
  libbergamot-translator.a → libmarian.a → (intgemm / yaml-cpp / phf /
  spdlog / sentencepiece / …) → libssplit.a → libpcre2-8.a
ruy 更是被拆成 30+ 个接口库。

Rust 侧要链接它们，就得知道「有哪些库、分别在哪、顺序如何」。
本脚本从 `build.ninja` 解析出这张图，导出成一份清单，
由 `core/build.rs` 读取并翻译成 `rustc-link-search` / `rustc-link-lib`。

## 为什么用清单而不是「合并成一个 .a」

试过 `llvm-ar -M`（MRI 模式）合并，但在 Windows 上有两个坑：
1. **`llvm-ar -M` 不能带位置参数**（脚本必须从 stdin 读），
   且输出归档名由脚本里的 `CREATE` 指定；
2. ⚠️⚠️ **llvm-ar 在含非 ASCII 字符的路径下会静默失败**
   （rc=0、无报错、输出文件不生成）—— 本仓库路径
   `C:\Users\Admin\WorkBuddy\代码开发\…` 正好命中。

绕开这些坑比硬啃 MRI 更划算：静态链接顺序由 CMake 保证，
Rust 侧用 `+whole-archive` 拉主库、其余按序链接即可。

## 依赖清单从哪来

CMake 的 Ninja 生成器把**传递依赖**写在每个 `build` 行的 `||` 之后：
    build src/translator/libbergamot-translator.a: CXX_STATIC_LIBRARY_LINKER__…
        …obj… || …/libruy_*.a …/libsentencepiece.a libmarian.a libssplit.a

这个顺序就是**正确的静态链接顺序**（引用者在前、被引用者在后）。
从 `libbergamot-translator.a` 出发做 BFS 并保持发现顺序即可。

## 输出格式

每行一个**绝对路径**的 .a（纯文本，UTF-8）。Rust 侧按行读取。
空行与 `#` 开头的行会被跳过。

## 用法

    python gen_bergamot_libs.py <build_dir> <输出清单路径>
"""

import os
import sys


def collect_libs(build_dir: str, entry: str):
    """从 build.ninja 解析依赖图，返回库相对路径列表（保持链接顺序）。"""
    ninja_path = os.path.join(build_dir, "build.ninja")
    if not os.path.isfile(ninja_path):
        sys.exit(f"[ERROR] 找不到 {ninja_path}")

    with open(ninja_path, "r", encoding="utf-8", errors="replace") as f:
        lines = f.read().splitlines()

    # 目标（.a）→ 其 || 之后的依赖
    deps: dict[str, list[str]] = {}
    for line in lines:
        if not line.startswith("build "):
            continue
        head, _, tail = line.partition(":")
        outs = head[len("build "):].strip().split(" ")
        if "||" not in tail:
            continue
        dep_part = tail.split("||", 1)[1]
        for o in outs:
            if o.endswith(".a"):
                deps[o] = dep_part.split()

    # BFS：保持发现顺序（CMake 的拓扑序 = 正确的静态链接顺序）
    seen: set[str] = set()
    order: list[str] = []
    queue = [entry]
    while queue:
        cur = queue.pop(0)
        if cur in seen:
            continue
        seen.add(cur)
        if cur.endswith(".a"):
            order.append(cur)
        for d in deps.get(cur, []):
            if d not in seen:
                queue.append(d)

    out = []
    for lib in order:
        p = os.path.join(build_dir, lib)
        if os.path.isfile(p):
            out.append(lib)
        else:
            print(f"[WARN] 依赖不存在，跳过：{lib}", file=sys.stderr)

    # ⚠️ **pcre2 不在依赖图里**，必须手动补。
    #
    # pcre2 是 **ExternalProject**：CMake 不把它建成普通 target，而是
    # 在构建结束时 install 到 `<build>/lib/libpcre2-8.a`。
    # 于是 `build.ninja` 里**没有任何 target 依赖它**，
    # BFS 自然收集不到 → 链接期报一堆
    # "undefined symbol: pcre2_compile_8 / pcre2_match_8 / …"。
    #
    # 它是 ssplit-cpp 的正则引擎依赖，必须链。
    pcre2 = os.path.join(build_dir, "lib", "libpcre2-8.a")
    if os.path.isfile(pcre2):
        out.append(os.path.join("lib", "libpcre2-8.a"))
    else:
        print("[WARN] 找不到 libpcre2-8.a（ExternalProject 未 install？）", file=sys.stderr)

    return out


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)

    build_dir = sys.argv[1]
    out_list = sys.argv[2]

    entry = "src/translator/libbergamot-translator.a"
    libs = collect_libs(build_dir, entry)
    if not libs:
        sys.exit("[ERROR] 未收集到任何静态库")

    print(f"[INFO] 依赖图共 {len(libs)} 个静态库", file=sys.stderr)

    with open(out_list, "w", encoding="utf-8", newline="\n") as f:
        for lib in libs:
            f.write(os.path.abspath(os.path.join(build_dir, lib)) + "\n")

    print(f"[PASS] 链接清单已写入：{out_list}", file=sys.stderr)
    print(out_list)


if __name__ == "__main__":
    main()
