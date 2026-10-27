#!/usr/bin/env bash
# 获取（或校验）sherpa-onnx 官方 Android 预编译包。
#
# 落点：translator_core/third_party/sherpa-prebuilt/android/jniLibs/
# 版本：与 core/Cargo.lock 里 sherpa-onnx-sys 的实锁版本一致（1.13.6）。
#      版本不匹配时链接期符号缺失，比不链更难排查，故校验 lock 版本。
#
# 若该目录已存在且完整，本脚本仅做校验并退出 0（幂等）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$AND_DIR/.." && pwd)"
REPO="$ROOT/translator_core"
DEST="$REPO/third_party/sherpa-prebuilt/android"
SEVENZ="/c/Program Files/7-Zip/7z.exe"

# ── 版本必须与 Cargo.lock 一致 ─────────────────────────
LOCK="$REPO/core/Cargo.lock"
[[ -f "$LOCK" ]] || { echo "[FAIL] 缺 Cargo.lock: $LOCK" >&2; exit 1; }
VERSION="$(grep -A 1 'name = "sherpa-onnx-sys"' "$LOCK" | grep '^version' | sed 's/.*"\(.*\)".*/\1/')"
[[ -n "$VERSION" ]] || { echo "[FAIL] Cargo.lock 里找不到 sherpa-onnx-sys 版本" >&2; exit 1; }
echo "[INFO] 目标版本：$VERSION（取自 Cargo.lock）"

ARCHIVE="sherpa-onnx-v${VERSION}-android.tar.bz2"
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/v${VERSION}/${ARCHIVE}"

REQUIRED_LIBS=(libonnxruntime.so libsherpa-onnx-c-api.so)

check_complete() {
  local abi="$1"
  for lib in "${REQUIRED_LIBS[@]}"; do
    [[ -f "$DEST/jniLibs/$abi/$lib" ]] || return 1
  done
  return 0
}

# ── 幂等：已齐则只校验 ─────────────────────────────────
if check_complete arm64-v8a; then
  echo "[PASS] 包已就位：$DEST/jniLibs/arm64-v8a"
  ls -la "$DEST/jniLibs/arm64-v8a" | grep "\.so$"
  exit 0
fi

[[ -x "$SEVENZ" ]] || { echo "[FAIL] 需要 7-Zip：$SEVENZ（Windows 自带 tar 解 .tar.bz2 不可靠）" >&2; exit 1; }

mkdir -p "$DEST"
cd "$DEST"
echo "[INFO] 下载 $URL"
curl -L --retry 3 -o "$ARCHIVE" "$URL"

echo "[INFO] 解包（bz2 → tar → jniLibs）"
"$SEVENZ" x -y "$ARCHIVE" >/dev/null
"$SEVENZ" x -y "${ARCHIVE%.bz2}" >/dev/null
rm -f "$ARCHIVE" "${ARCHIVE%.bz2}"

if ! check_complete arm64-v8a; then
  echo "[FAIL] 解包后 jniLibs/arm64-v8a 不完整" >&2
  exit 1
fi
echo "[PASS] 完成：$DEST/jniLibs/arm64-v8a"
ls -la "$DEST/jniLibs/arm64-v8a" | grep "\.so$"
