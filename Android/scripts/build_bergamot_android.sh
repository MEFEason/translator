#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# Bergamot 翻译引擎 · Android arm64 交叉编译
# ─────────────────────────────────────────────────────────────
#
# ## 为什么需要这个脚本
#
# Translator 的 Rust 核心在 Android 上要链接 Bergamot（Marian 权重格式的
# 离线翻译引擎）。Bergamot 是 C++ 项目，Windows 端用的是**预先编好的
# MSVC 静态库**（third_party/bergamot-translator/build/），
# 而 Android 需要 **arm64-v8a** 版本 —— 必须用 NDK 工具链交叉编译。
#
# ## 离线约束
#
# 构建期允许克隆上游源码（构建工具链不属于产品运行时），
# 但**产物必须完全离线可运行** —— Translator 是硬约束「运行期零网络」。
#
# ## 关键 CMake 选项（踩坑记录）
#
# - BUILD_ARCH=armv8-a：marian 把它直接拼进 `-march=${BUILD_ARCH}`。
#   ⚠️ **不能写 arm64** —— clang 不认（`unsupported argument 'arm64'
#   to option '-march='`），NDK 的正确值是 `armv8-a`。
#   ⚠️ 也不能留默认的 "native"：交叉编译时它会在**构建机**上跑 CPU
#   指令集探测，得出 x86 结论 → 编出含 x86 指令的 arm64 二进制，
#   运行时 SIGILL。
#   架构本身（ARM64）由编译器宏 `__aarch64__` 判定（marian 的
#   cmake/TargetArch.cmake），不依赖这个值 → 用 armv8-a 无副作用。
# - USE_SENTENCEPIECE=ON：Marian 需要 sentencepiece 分词。
# - COMPILE_CUDA=OFF：没有 CUDA 工具链。
# - COMPILE_TESTS=OFF / COMPILE_LIBRARY_ONLY=ON：只要库，不要 app 和测试。
# - USE_MKL=OFF：MKL 是 Intel 专有的，ARM 上没有。
# - USE_STATIC_LIBS=ON：静态链接，产物只有一个 .a，避免运行时找 .so。
#
# ## 用法
#
#   bash Android/scripts/build_bergamot_android.sh
#
# 产物：
#   translator_core/third_party/bergamot-translator/build-android/
#     src/translator/libbergamot-translator.a
#
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BERGAMOT_SRC="$REPO/translator_core/third_party/bergamot-translator"
BUILD_DIR="$BERGAMOT_SRC/build-android"

# ── NDK ───────────────────────────────────────────────────
# ⚠️ 路径要覆盖 NDK r27c（与项目其它环节一致）。
#    NDK 自带的 cmake 就在 $NDK/build/cmake/ 下。
NDK="${ANDROID_NDK_HOME:-/c/Program Files (x86)/Android/AndroidNDK/android-ndk-r27c}"
NDK_WIN="$(cygpath -w "$NDK" 2>/dev/null || echo "$NDK")"

API=29                       # 与 app/build.gradle.kts 的 minSdk 一致
# ⚠️ ANDROID_ABI 要的是 **Android ABI 名**（arm64-v8a），
#    不是 clang triplet（aarch64-linux-android）—— 写错会报
#    "Invalid Android ABI"。triplet 只用于选编译器前缀。
ABI="arm64-v8a"
JOBS="${JOBS:-8}"

# ── cmake ─────────────────────────────────────────────────
# 优先用 NDK 自带的 cmake（版本与 NDK 匹配，避免兼容问题）。
if [ -x "$NDK/build/cmake/bin/cmake.exe" ]; then
  CMAKE="$NDK/build/cmake/bin/cmake.exe"
elif [ -x "/c/Program Files/Microsoft Visual Studio/18/Professional/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe" ]; then
  CMAKE="/c/Program Files/Microsoft Visual Studio/18/Professional/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe"
else
  echo "[ERROR] 找不到 cmake（NDK 自带或 VS 自带都行）" >&2
  exit 1
fi

echo "[INFO] NDK      = $NDK"
echo "[INFO] cmake    = $CMAKE"
echo "[INFO] source   = $BERGAMOT_SRC"
echo "[INFO] build    = $BUILD_DIR"
echo "[INFO] abi     = $ABI (API $API)"

# ── ninja ────────────────────────────────────────────────
# cmake -G Ninja 需要 ninja.exe。VS 自带一份（与 cmake 同目录树下）。
NINJA="/c/Program Files/Microsoft Visual Studio/18/Professional/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja/ninja.exe"
if [ ! -x "$NINJA" ]; then
  NINJA="$(command -v ninja || true)"
fi
if [ -z "$NINJA" ] || [ ! -x "$NINJA" ]; then
  echo "[ERROR] 找不到 ninja（VS 自带或 PATH 里都行）" >&2
  exit 1
fi
export PATH="$(dirname "$NINJA"):$PATH"
echo "[INFO] ninja    = $NINJA"

# ⚠️ 环境里可能已存在 ANDROID_NDK（指向**别的** NDK 版本），
#    NDK 的 android.toolchain.cmake 会优先用它 → 与我们选的
#    toolchain 文件所属 NDK 不一致 → "Invalid Android ABI" /
#    "No CMAKE_CXX_COMPILER could be found"。
#    显式覆盖成同一个 NDK。
export ANDROID_NDK="$NDK_WIN"

# ── 1. 确保子模块已就位 ──────────────────────────────────
# marian-dev 及其依赖（intgemm / sentencepiece / ruy）是 git submodule。
# Windows 端已把它们拉下来了，但**新克隆的仓库**里可能是空的。
ensure_submodule() {
  local path="$1" url="$2"
  if [ -d "$path" ] && [ -n "$(ls -A "$path" 2>/dev/null)" ]; then
    return 0
  fi
  echo "[INFO] 拉取 $url ..."
  if [ -d "$path/.git" ] || [ -f "$path/.git" ]; then
    git -C "$path" submodule update --init --recursive --depth 1 || true
  else
    git clone --depth 1 --recurse-submodules "$url" "$path" || true
  fi
}

# marian-dev 本身
ensure_submodule "$BERGAMOT_SRC/3rd_party/marian-dev" \
  "https://github.com/browsermt/marian-dev"
# marian-dev 的子模块（intgemm / sentencepiece / ruy 是关键）
if [ -d "$BERGAMOT_SRC/3rd_party/marian-dev/src/3rd_party" ]; then
  for m in intgemm sentencepiece ruy; do
    ensure_submodule "$BERGAMOT_SRC/3rd_party/marian-dev/src/3rd_party/$m" \
      "https://github.com/$(case $m in
          intgemm) echo kpu/intgemm ;;
          sentencepiece) echo browsermt/sentencepiece ;;
          ruy) echo google/ruy ;;
        esac)"
  done
fi
# ssplit-cpp（分段器）
ensure_submodule "$BERGAMOT_SRC/3rd_party/ssplit-cpp" \
  "https://github.com/browsermt/ssplit-cpp"

# ── 2. CMake configure ───────────────────────────────────
# ⚠️ cmake 4.x 拒绝 `cmake_minimum_required(VERSION 3.5.1)`
#    （低于 3.5 的版本被移除）→ 用 CMAKE_POLICY_VERSION_MINIMUM 兜底。
TOOLCHAIN="$NDK/build/cmake/android.toolchain.cmake"

mkdir -p "$BUILD_DIR"

# ⚠️ **这里不删任何东西**（曾经写过 rm -rf，被安全策略拦下：
#    批量删除 ≥50 个文件需要人工确认，会让构建脚本直接失败）。
#
#    为什么原本要删：
#    - 主工程的 CMakeCache.txt：参数变了（ABI / NDK / 生成器）时，
#      CMake 不会自动重置 → 必须从干净状态重来。
#      → 解决办法：**换个构建目录**（见下），而不是删。
#    - pcre2 这个 ExternalProject 的独立 build 目录：换生成器时会报
#      "generator Ninja does not match the generator used previously"。
#      → 解决办法：FindPCRE2.cmake 已修（继承主工程生成器），
#        只有**历史遗留**的 VS cache 会冲突；用 --clean 手动清一次即可。
#
# 生成器冲突检测：主工程 cache 里的生成器与本次要用的不同 → 换目录。
GENERATOR="Ninja"
if [[ -f "$BUILD_DIR/CMakeCache.txt" ]]; then
  OLD_GEN="$(grep -m1 '^CMAKE_GENERATOR:INTERNAL=' "$BUILD_DIR/CMakeCache.txt" 2>/dev/null | cut -d= -f2)"
  if [[ -n "$OLD_GEN" && "$OLD_GEN" != "$GENERATOR" ]]; then
    echo "[INFO] 检测到生成器变更（$OLD_GEN → $GENERATOR），换用新构建目录"
    BUILD_DIR="${BUILD_DIR}-${GENERATOR,,}"
  fi
fi

"$CMAKE" -S "$BERGAMOT_SRC" -B "$BUILD_DIR" \
  -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN" \
  -DANDROID_ABI="$ABI" \
  -DANDROID_PLATFORM="android-$API" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DBUILD_ARCH=armv8-a \
  -DCOMPILE_CUDA=OFF \
  -DCOMPILE_TESTS=OFF \
  -DCOMPILE_LIBRARY_ONLY=ON \
  -DCOMPILE_PYTHON=OFF \
  -DUSE_MKL=OFF \
  -DUSE_STATIC_LIBS=ON \
  -DUSE_SENTENCEPIECE=ON \
  -DSSPLIT_COMPILE_LIBRARY_ONLY=ON \
  -DSSPLIT_USE_INTERNAL_PCRE2=ON \
  -DUSE_WASM_COMPATIBLE_SOURCE=OFF \
  2>&1 | tail -40

echo "[INFO] configure 完成，开始编译 ..."
"$CMAKE" --build "$BUILD_DIR" --parallel "$JOBS" 2>&1 | tail -30

# ── 3. 产物检查 ──────────────────────────────────────────
LIB_DIR="$BUILD_DIR/src/translator"
FOUND=""
for f in "$LIB_DIR"/libbergamot-translator.a "$LIB_DIR"/libbergamot_translator.a; do
  [ -f "$f" ] && FOUND="$f" && break
done
if [ -z "$FOUND" ]; then
  echo "[ERROR] 未找到编译产物，列出 $LIB_DIR 内容：" >&2
  ls -la "$LIB_DIR" 2>/dev/null >&2 || true
  exit 1
fi

SIZE=$(du -h "$FOUND" | cut -f1)
echo "[PASS] Bergamot arm64 静态库: $FOUND ($SIZE)"
echo "[INFO] 下一步：build.rs 需链接该库（BERGAMOT_PREBUILT / Android 路径）"
