#!/usr/bin/env bash
# Windows 桌面版构建：translator_core → translator_core.dll
#
# ## 产物去向
#   ui/windows/rust/translator_core.dll
#   （顶层 windows/CMakeLists.txt 会 install 到 exe 同目录）
#
# ## 最低系统要求：Windows 10（Flutter 官方支持范围）
# - Flutter 3.x 桌面端不再支持 Windows 7/8（含 8.1）
# - flexaudio 的 WASAPI loopback 需 Win10 1903+ 才能按进程排除自身
#   （退化为整块 loopback 也能工作，只是可能回采自己的提示音）
#
# ## 为什么必须带 capture-desktop
# 不带这个 feature 时 `platform::create_capture` 直接返回
# CaptureUnavailable —— Windows 上就没有任何音频来源。
#
# 用法：
#   scripts/build_windows.sh              # release（默认，配 flutter build windows）
#   scripts/build_windows.sh --debug      # debug
#   scripts/build_windows.sh --clean      # 先清理
#   scripts/build_windows.sh --with-asr   # 额外启用 sherpa-onnx ASR 后端

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WIN_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$WIN_DIR/.." && pwd)"
REPO="$ROOT/translator_core"
CORE="$REPO/core"
OUT_DIR="$REPO/ui/windows/rust"

export PATH="$HOME/.cargo/bin:$PATH"

# ── 参数 ──────────────────────────────────────────────
PROFILE="release"
CARGO_FLAGS="--release"
FEATURES="capture-desktop"
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE="release"; CARGO_FLAGS="--release" ;;
    --debug)   PROFILE="debug";   CARGO_FLAGS="" ;;
    --clean)   CARGO_FLAGS="$CARGO_FLAGS --clean" ;;
    --with-asr) FEATURES="$FEATURES,asr-sherpa" ;;
    *) echo "未知参数: $arg" >&2; exit 1 ;;
  esac
done

# ── 前置检查 ──────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
  cat >&2 <<'EOF'
[FAIL] 找不到 cargo。

Rust 装好后重试；或确认 ~/.cargo/bin 已在 PATH 中。
  https://rustup.rs
EOF
  exit 1
fi

echo "[INFO] rustc: $(cargo --version 2>/dev/null || rustc --version)"

# 非 Windows 主机交叉编译 Windows DLL 需要额外的 target 与 linker，
# 本脚本只覆盖「在 Windows 上直接构建」这一条主路径。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) : ;;
  *)
    cat >&2 <<'EOF'
[FAIL] 本脚本需在 Windows 上直接构建（Rust msvc 工具链）。
macOS / Linux 上交叉编译 Windows DLL 还要额外装
  rustup target add x86_64-pc-windows-msvc
与 LLVM lld-link，届时再补。
EOF
    exit 1
    ;;
esac

# ── 构建 ──────────────────────────────────────────────
CORES="$( (nproc 2>/dev/null || echo 4) )"
JOBS=$(( CORES > 1 ? CORES - 1 : 1 ))

echo "[INFO] 构建 profile=$PROFILE  features=$FEATURES  jobs=$JOBS"
( cd "$CORE" && cargo build $CARGO_FLAGS --features "$FEATURES" -j "$JOBS" )

# ── 收集产物 ──────────────────────────────────────────
DLL="$CORE/target/$PROFILE/translator_core.dll"
if [[ ! -f "$DLL" ]]; then
  echo "[FAIL] 未找到产物: $DLL" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
cp -f "$DLL" "$OUT_DIR/translator_core.dll"
SIZE="$(du -h "$OUT_DIR/translator_core.dll" | cut -f1)"
echo "[PASS] 已生成 $OUT_DIR/translator_core.dll ($SIZE)"

# ── 校验：PE 架构 + FFI 导出符号 ──────────────────────
echo ""
echo "[INFO] 校验 PE 架构"
PY=""
for cand in python3 python py; do
  if command -v "$cand" >/dev/null 2>&1; then PY="$cand"; break; fi
done
if [[ -n "$PY" ]]; then
  "$PY" - "$OUT_DIR/translator_core.dll" <<'PYEOF'
import re, sys
data = open(sys.argv[1], "rb").read()

# PE 头：e_lfanew 在 0x3C 处
pe = int.from_bytes(data[0x3C:0x40], "little")
assert data[pe:pe+4] == b"PE\0\0", "不是合法的 PE 文件"
machine = int.from_bytes(data[pe+4:pe+6], "little")
names = {0x8664: "x64", 0x14C: "x86", 0xAA64: "ARM64"}
print(f"  架构: {names.get(machine, hex(machine))}")
if machine != 0x8664:
    print("  [WARN] Flutter Windows 只支持 x64")

# Optional Header 的 SubsystemVersion 即最低系统版本（10.0 = Win10）
opt = pe + 24
major = int.from_bytes(data[opt+40:opt+42], "little")
minor = int.from_bytes(data[opt+42:opt+44], "little")
print(f"  SubsystemVersion: {major}.{minor}"
      + ("（声明可在 Win7+ 加载，无硬性下限）" if (major, minor) <= (6, 0)
         else "（声明的最低系统版本偏高，确认目标机器可满足）"))
print("  实际下限由所用 API 决定：Flutter 桌面端要求 Windows 10 64 位")

syms = sorted(set(re.findall(rb"translator_[a-z_]+", data)))
print(f"  FFI 导出符号: {len(syms)} 个")
if len(syms) < 10:
    print("  [WARN] 导出符号偏少，Dart 侧可能找不到入口")
PYEOF
fi

echo ""
echo "[INFO] 动态依赖（不应有第三方运行时）"
if command -v objdump >/dev/null 2>&1; then
  objdump -p "$OUT_DIR/translator_core.dll" 2>/dev/null | grep "DLL Name" || true
else
  echo "  （objdump 不可用，跳过；MSVC 构建只依赖系统 dll）"
fi

echo ""
echo "下一步："
echo "  cd ui && flutter build windows --release"
echo "  产物: ui/build/windows/x64/runner/Release/translator.exe"