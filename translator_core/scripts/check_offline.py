#!/usr/bin/env python3
"""离线性门禁（三层）。

## 「离线」指的是什么

**交付物运行时离线、开箱即用**，不是编译期离线。

构建机可以联网 —— `sherpa-onnx-sys` 的 build script 会从 GitHub Releases
下载预编译静态库（首次构建 115MB，之后有缓存）。这是构建期的一次性成本，
不进产物、不影响用户。

因此本脚本分三层，各管一段：

| 层 | 管什么 | 为什么单独一层 |
|---|---|---|
| 1. 源码审计 | 我们自己写的代码里有没有网络调用 | 秒级反馈，比 `cargo tree` 快 100 倍；且**依赖树能被绕过**（vendoring、feature 组合），源码里的网络调用无法伪装 |
| 2. 运行期依赖断言 | 进产物的依赖里有没有网络 crate | 不可协商的红线 |
| 3. 构建期白名单 | 构建期联网的 crate 是否在登记表内 | 允许构建期联网，但**必须登记且引入者匹配** —— 防止有人加了 reqwest 再顺手加白名单 |

## 历史（两个都踩过的坑）

1. 旧版用 `cargo tree --all-features --edges build`，把 `sherpa-onnx-sys` 的
   build-dependency（ureq/rustls/ring/url/webpki-roots 等 6 项）误判为
   运行时风险，打开 `asr-sherpa` 后必然 FAIL。根因是参数用错：
   `ureq` 只在 `[build-dependencies]` 里，`--edges normal` 的树中不含它。

2. **更危险的坑**：本机 cargo 上 `--edges build` **只输出根 crate**
   （实测 1 行，零 build 依赖）—— 也就是说用它做门禁会**永远 PASS**，
   是个假绿的检查。必须用 `--edges all` 取全集，
   再减掉 `--edges normal` 的运行期集，剩下的才是纯构建期依赖。
   **写门禁脚本必须验证它真的能看见东西**，否则比没有门禁更糟。

用法：
    python scripts/check_offline.py
    python scripts/check_offline.py --verbose     # 显示每层细节

退出码：0 通过 / 1 失败
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CORE = REPO / "core"

# ── 层 1：源码里的网络调用特征 ──────────────────────────
#
# 故意宽松（宁可多报）：漏报的代价是用户拿到会联网的"离线"软件，
# 误报只是人工确认一下。
SOURCE_PATTERNS = [
    # Rust 标准库网络
    (r"\bstd::net\b", "Rust 标准库网络"),
    (r"\bTcpStream\b", "TCP 连接"),
    (r"\bUdpSocket\b", "UDP socket"),
    (r"\bTcpListener\b", "TCP 监听"),
    # 常见 HTTP 客户端（直接出现在源码里 = 绕过依赖审计）
    (r"\b(reqwest|hyper|ureq|isahc|attohttpc|minreq)\s*::", "HTTP 客户端"),
    (r"\bHttpClient\b", "HTTP 客户端"),
    # Win32 网络 API
    (r"\bWinHttpOpen\b", "WinHTTP"),
    (r"\bInternetOpenA?\b", "WinINet"),
    (r"\bWSAStartup\b", "Winsock"),
    # Kotlin / Android
    (r"\bHttpURLConnection\b", "Java HTTP"),
    (r"\bokhttp3\b", "OkHttp"),
    (r"\bRetrofit\b", "Retrofit"),
    # Dart
    (r"package:http/", "Dart http 包"),
    (r"\bHttpClient\s*\(", "Dart HttpClient"),
    (r"\bSocket\.connect\b", "Dart Socket"),
    (r"\bWebSocket\.connect\b", "Dart WebSocket"),
]

# 层 1 扫描的文件范围
SOURCE_GLOBS = [
    (CORE / "src", "*.rs"),
    (REPO / "ui" / "lib", "*.dart"),
    (REPO / "ui" / "android", "*.kt"),
    (REPO / "ui" / "windows", "*.cpp"),
    (REPO / "ui" / "windows", "*.h"),
]

# 层 1 豁免：构建脚本**允许**联网（构建期一次性下载）
SOURCE_EXEMPT = {
    "build.rs",
}

# ── 层 2：运行期禁止的网络 crate ────────────────────────
FORBIDDEN_RUNTIME = {
    # HTTP 客户端
    "reqwest", "hyper", "hyper-tls", "hyper-util", "curl", "curl-sys",
    "ureq", "isahc", "attohttpc", "surf", "http", "http-body",
    "tiny_http", "minreq", "ureq-proto", "url", "urlencoding",
    # TLS / 证书
    "rustls", "native-tls", "openssl", "openssl-sys", "ring",
    "webpki-roots", "rustls-webpki", "webpki", "rustls-pki-types",
    # 异步网络运行时 / Web 框架
    "tokio-tungstenite", "tokio-native-tls", "tower-http", "axum",
    "actix-web", "warp", "rocket", "salvo", "tonic", "poem",
    # 在线模型 / API / 遥测
    "hf-hub", "huggingface-hub", "sentry", "sentry-types",
    "opentelemetry", "posthog", "segment", "analytics",
    # 会联网取二进制的下载器
    "ureq-download", "fetch", "download",
}

# ── 层 3：构建期白名单 ──────────────────────────────────
#
# 登记格式：crate 名 → 允许的引入者（父 crate 名）。
# 反查 `cargo tree -i <crate> --edges build` 的引入者，不在表内即 FAIL。
#
# 目前唯一登记项：sherpa-onnx-sys 的 build script 从 GitHub Releases
# 下载预编译静态库（首次 115MB，之后命中 target/sherpa-onnx-prebuilt 缓存）。
BUILD_WHITELIST: dict[str, set[str]] = {
    "ureq": {"sherpa-onnx-sys"},
    "ureq-proto": {"sherpa-onnx-sys"},
    "url": {"sherpa-onnx-sys", "ureq"},
    "webpki-roots": {"ureq"},
    "rustls": {"ureq"},
    "rustls-pki-types": {"rustls"},
    "webpki": {"rustls"},
    "ring": {"rustls"},
    # rustls 的证书校验实现（rustls-webpki ← rustls ← ureq ←
    # sherpa-onnx-sys 的 build-dependencies，整链同源）
    "rustls-webpki": {"rustls"},
}

# 运行期依赖必须显式列举 feature —— 不用 --all-features
# （那会把构建期依赖混进运行期树，制造假阳性）
RUNTIME_FEATURES = "capture-desktop,asr-sherpa,mt-bergamot"

VERBOSE = "--verbose" in sys.argv or "-v" in sys.argv


def log(msg: str) -> None:
    if VERBOSE:
        print(f"    {msg}")


def cargo(args: list[str], edges: str | None = None):
    cmd = ["cargo", *args]
    if edges:
        cmd += ["--edges", edges]
    try:
        r = subprocess.run(
            cmd, cwd=CORE, capture_output=True, text=True, timeout=300
        )
        return r.stdout if r.returncode == 0 else None
    except (subprocess.SubprocessError, FileNotFoundError, OSError) as e:
        log(f"cargo {' '.join(args)} 失败: {e}")
        return None


def parse_packages(tree_text: str) -> set[str]:
    """提取 `name vX.Y.Z` 形式的包名。"""
    return {m.group(1) for m in re.finditer(r"([A-Za-z0-9_.-]+)\s+v[0-9]", tree_text)}


# ── 层 1：源码审计 ──────────────────────────────────────

def check_sources() -> bool:
    print("\n[层1] 源码审计：我们自己写的代码不得发起网络请求")

    hits: list[tuple[Path, int, str, str]] = []
    scanned = 0

    for root, pattern in SOURCE_GLOBS:
        if not root.exists():
            continue
        for f in sorted(root.rglob(pattern)):
            if f.name in SOURCE_EXEMPT:
                log(f"豁免（构建脚本允许联网）: {f.name}")
                continue
            # target/ 与 build/ 里是生成物，不是我们写的
            if any(p in f.parts for p in ("target", "build", ".dart_tool")):
                continue
            scanned += 1
            try:
                text = f.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                # 注释里的说明不算（但会把命中降级为提示）
                stripped = line.strip()
                is_comment = (
                    stripped.startswith("//")
                    or stripped.startswith("#")
                    or stripped.startswith("*")
                    or stripped.startswith("///")
                )
                for pat, desc in SOURCE_PATTERNS:
                    if re.search(pat, line):
                        if is_comment:
                            log(f"注释中的提及（不计）: {f}:{lineno} {desc}")
                            continue
                        hits.append((f, lineno, desc, stripped[:90]))

    print(f"    扫描 {scanned} 个源文件")

    if not hits:
        print("    [PASS] 源码中无网络调用")
        return True

    print(f"    [FAIL] 发现 {len(hits)} 处网络调用特征:")
    for f, lineno, desc, snippet in hits[:20]:
        rel = f.relative_to(REPO) if f.is_relative_to(REPO) else f
        print(f"      - {rel}:{lineno}  [{desc}]  {snippet}")
    if len(hits) > 20:
        print(f"      … 另有 {len(hits) - 20} 处")
    return False


# ── 层 2：运行期依赖 ────────────────────────────────────

def check_runtime_deps() -> bool:
    print(f"\n[层2] 运行期依赖（--features {RUNTIME_FEATURES}）")

    tree_text = cargo(["tree", "--features", RUNTIME_FEATURES], edges="normal")
    if tree_text is None:
        print("    [FAIL] cargo tree 执行失败（cargo 未就绪或依赖未拉取）")
        return False

    packages = parse_packages(tree_text)
    hits = sorted(packages & FORBIDDEN_RUNTIME)

    if not hits:
        print(f"    [PASS] {len(packages)} 个包，无网络类依赖")
        return True

    print(f"    [FAIL] 发现 {len(hits)} 个网络类依赖:")
    for name in hits:
        # 反查引入链，告诉用户是谁带进来的
        chain = cargo(["-i", name], edges="normal")
        if chain:
            parents = [
                ln.strip()
                for ln in chain.splitlines()
                if ln.strip() and not ln.strip().startswith(name)
            ][:3]
            if parents:
                print(f"      - {name}  ← 引入者: {parents[0]}")
            else:
                print(f"      - {name}")
        else:
            print(f"      - {name}")
    return False


# ── 层 3：构建期白名单 ──────────────────────────────────

def check_build_deps() -> bool:
    print("\n[层3] 构建期依赖白名单")

    # ⚠️ 必须用 --edges all，不能用 --edges build：
    # 本机 cargo 上 --edges build 只输出根 crate（1 行，零依赖），
    # 用它做门禁会永远 PASS —— 假绿比没有门禁更危险。
    all_text = cargo(["tree", "--all-features"], edges="all")
    if all_text is None:
        print("    [FAIL] cargo tree --edges all 执行失败")
        return False

    # 全集减运行期集 = 纯构建期依赖
    normal_text = cargo(["tree", "--all-features"], edges="normal") or ""
    all_packages = parse_packages(all_text)
    runtime_packages = parse_packages(normal_text)
    build_only = all_packages - runtime_packages

    packages = build_only
    log(f"全集 {len(all_packages)} / 运行期 {len(runtime_packages)} "
        f"/ 纯构建期 {len(build_only)}")
    hits = sorted(packages & FORBIDDEN_RUNTIME)

    if not hits:
        print(f"    [PASS] {len(packages)} 个包，构建期也无网络类依赖")
        return True

    # 逐个核对白名单 + 反查引入者
    unlisted: list[str] = []
    wrong_parent: list[tuple[str, set[str]]] = []

    for name in hits:
        allowed_parents = BUILD_WHITELIST.get(name)
        if allowed_parents is None:
            unlisted.append(name)
            continue

        chain = cargo(["-i", name], edges="all")
        actual_parents: set[str] = set()
        if chain:
            for ln in chain.splitlines():
                # cargo tree -i 的每行是一个父节点
                m = re.match(r"^([A-Za-z0-9_.-]+)\s+v", ln.strip())
                if m and m.group(1) != name:
                    actual_parents.add(m.group(1))

        if actual_parents and not (actual_parents & allowed_parents):
            wrong_parent.append((name, actual_parents))
            continue

        print(f"    [OK] {name}（白名单内，引入者匹配 {sorted(allowed_parents)}）")

    ok = True
    if unlisted:
        print(f"    [FAIL] {len(unlisted)} 个未登记的构建期网络依赖:")
        for name in unlisted:
            chain = cargo(["-i", name], edges="all")
            parent = ""
            if chain:
                for ln in chain.splitlines():
                    m = re.match(r"^([A-Za-z0-9_.-]+)\s+v", ln.strip())
                    if m and m.group(1) != name:
                        parent = m.group(1)
                        break
            print(f"      - {name}" + (f"  ← {parent}" if parent else ""))
            print(f"        若确需保留，请在 BUILD_WHITELIST 登记并写明理由")
        ok = False

    if wrong_parent:
        print(f"    [FAIL] {len(wrong_parent)} 个引入者与登记不符:")
        for name, actual in wrong_parent:
            print(f"      - {name}: 登记 {sorted(BUILD_WHITELIST[name])}，"
                  f"实际 {sorted(actual)}")
        ok = False

    return ok


# ── Cargo.lock 存在性 ───────────────────────────────────

def check_lockfile() -> bool:
    if not (CORE / "Cargo.lock").exists():
        print("\n[FAIL] core/Cargo.lock 不存在")
        print("       离线构建需要锁文件才能保证可复现：cd core && cargo fetch")
        return False
    print("\n[OK] core/Cargo.lock 存在")
    return True


def main() -> int:
    print("=" * 62)
    print("离线性门禁（三层）")
    print("约束：交付物**运行时**离线、开箱即用（构建期可联网）")
    print("=" * 62)

    ok = True
    ok &= check_lockfile()
    ok &= check_sources()
    ok &= check_runtime_deps()
    ok &= check_build_deps()

    print("\n" + "-" * 62)
    if ok:
        print("[PASS] 三层全部通过：源码无网络调用、运行期无网络依赖、")
        print("       构建期联网项均在白名单内且引入者匹配")
        return 0
    print("[FAIL] 离线性检查未通过")
    return 1


if __name__ == "__main__":
    sys.exit(main())
